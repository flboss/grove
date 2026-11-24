use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::Utc;
use grove_exec::value::Val;
use grove_exec::{ExecError, ExecOptions, ExecSession};
use grove_query::ast::QueryFile;
use grove_query::{ParseOutcome, TypeBindings};
use grove_schema::ValidatedSchema;
use rustyline::error::ReadlineError;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::args::{ConnectionSpec, Format, OutputArgs, ReplArgs};
use crate::diagnostics;
use crate::error::{CliError, CliResult, EXIT_COMPILE};
use crate::render::{self, RenderOptions};

#[derive(Debug)]
enum ReplOutcome {
    Quit,
    ChangeBackend {
        spec: ConnectionSpec,
        schema: Option<(PathBuf, Box<ValidatedSchema>)>,
    },
    ReloadSchema {
        schema: Box<ValidatedSchema>,
        path: PathBuf,
    },
}

struct ReplSettings {
    output: OutputArgs,
    stats: bool,
}

struct ReplSession<'a> {
    schema: &'a ValidatedSchema,
    spec: &'a ConnectionSpec,
    schema_path: PathBuf,
    exec: ExecSession<'a>,
    types: TypeBindings,
    settings: &'a mut ReplSettings,
    exec_options: ExecOptions,
    turn: &'a mut usize,
}

pub(crate) fn run(args: ReplArgs) -> CliResult<()> {
    let ReplArgs {
        schema: mut schema_path,
        backend,
        output,
        no_pushdown,
    } = args;

    let mut spec = backend.resolve().map_err(CliError::usage)?;
    let (schema_name, schema_source) = crate::read_schema(&schema_path)?;
    let Some(mut schema) = crate::compile_schema(&schema_name, &schema_source) else {
        return Err(CliError::reported(EXIT_COMPILE));
    };

    let mut settings = ReplSettings {
        output,
        stats: false,
    };
    let mut turn = 0usize;
    let mut editor = rustyline::DefaultEditor::new()
        .map_err(|err| CliError::runtime(format!("failed to initialize REPL: {err}")))?;
    let history = history_path();
    if let Some(path) = &history
        && path.is_file()
    {
        let _ = editor.load_history(path);
    }
    let exec_options = ExecOptions {
        pushdown: !no_pushdown,
        materialize_lets: true,
        read_only: spec.is_read_only(),
        debug_rollback: false,
    };

    let result = repl_loop(
        &mut schema,
        &mut spec,
        &mut settings,
        &mut schema_path,
        exec_options,
        &mut turn,
        &mut editor,
    );

    if let Some(path) = &history {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let _ = editor.save_history(path);
    }

    result
}

fn repl_loop(
    schema: &mut ValidatedSchema,
    spec: &mut ConnectionSpec,
    settings: &mut ReplSettings,
    schema_path: &mut PathBuf,
    exec_options: ExecOptions,
    turn: &mut usize,
    editor: &mut rustyline::DefaultEditor,
) -> CliResult<()> {
    loop {
        let outcome = {
            let backend = spec
                .open(schema)
                .map_err(|err| CliError::runtime(format!("failed to open storage: {err}")))?;
            print_banner(spec, schema_path.as_path());

            let mut session = ReplSession {
                schema,
                spec,
                schema_path: schema_path.clone(),
                exec: ExecSession::new(backend.as_ref(), schema),
                types: TypeBindings::new(),
                settings,
                exec_options: ExecOptions {
                    read_only: spec.is_read_only(),
                    ..exec_options
                },
                turn,
            };
            session.run(editor)?
        };

        match outcome {
            ReplOutcome::Quit => break,
            ReplOutcome::ChangeBackend {
                spec: new_spec,
                schema: new_schema,
            } => {
                *spec = new_spec;
                if let Some((path, new_schema)) = new_schema {
                    *schema = *new_schema;
                    *schema_path = path;
                }
            }
            ReplOutcome::ReloadSchema {
                schema: new_schema,
                path,
            } => {
                *schema = *new_schema;
                *schema_path = path;
            }
        }
    }

    Ok(())
}

fn print_banner(spec: &ConnectionSpec, schema_path: &Path) {
    println!(
        "grove {} — {} — schema: {}",
        env!("CARGO_PKG_VERSION"),
        spec.description(),
        schema_path.display()
    );
    println!("Type \".help\" for a list of commands.");
}

impl<'a> ReplSession<'a> {
    fn run(&mut self, editor: &mut rustyline::DefaultEditor) -> CliResult<ReplOutcome> {
        let mut buffer = String::new();

        loop {
            let (prompt, continuation) = self.prompts();
            let active_prompt = if buffer.is_empty() {
                prompt.as_str()
            } else {
                continuation.as_str()
            };
            match editor.readline(active_prompt) {
                Ok(line) => {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        let _ = editor.add_history_entry(line.as_str());
                    }

                    if buffer.is_empty() && trimmed.starts_with('.') {
                        if let Some(outcome) = self.meta_command(trimmed)? {
                            return Ok(outcome);
                        }
                        continue;
                    }

                    if !buffer.is_empty() {
                        buffer.push('\n');
                    }
                    buffer.push_str(&line);

                    if buffer.trim().is_empty() {
                        buffer.clear();
                        continue;
                    }

                    let source_name = format!("<repl:{}>", *self.turn);
                    match grove_query::parse_query_repl(&buffer) {
                        ParseOutcome::Complete { file, diagnostics } => {
                            *self.turn += 1;
                            diagnostics::print(&buffer, &source_name, &diagnostics);
                            let _ = self.execute_file(file, &buffer, &source_name)?;
                            buffer.clear();
                        }
                        ParseOutcome::Incomplete { .. } => {}
                        ParseOutcome::Invalid { diagnostics } => {
                            *self.turn += 1;
                            diagnostics::print(&buffer, &source_name, &diagnostics);
                            buffer.clear();
                        }
                    }
                }
                Err(ReadlineError::Interrupted) => {
                    buffer.clear();
                }
                Err(ReadlineError::Eof) => {
                    if buffer.is_empty() {
                        return Ok(ReplOutcome::Quit);
                    }
                    buffer.clear();
                }
                Err(error) => {
                    return Err(CliError::runtime(format!("REPL input error: {error}")));
                }
            }
        }
    }

    fn meta_command(&mut self, line: &str) -> CliResult<Option<ReplOutcome>> {
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

    fn execute_source(&mut self, source: &str, name: &str) -> CliResult<Option<Val>> {
        let (file, diagnostics) = grove_query::parse_query(source);
        diagnostics::print(source, name, &diagnostics);
        let Some(file) = file else {
            return Ok(None);
        };
        self.execute_file(file, source, name)
    }

    fn execute_file(
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

    fn read_file(&mut self, path: &str) -> CliResult<()> {
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

    fn prompts(&self) -> (String, String) {
        let colored = self.settings.output.repl_color_enabled();
        let schema_name = truncated_file_name(&self.schema_path, 24);
        let mut prompt = String::new();
        let mut prompt_width = 0;

        const PROMPT_GROVE: &str = "grove";
        prompt.push_str(&render::paint(PROMPT_GROVE, Some("1;36"), colored));
        prompt.push(' ');
        prompt.push_str(&render::paint(schema_name.as_ref(), Some("1;32"), colored));
        prompt.push(' ');
        prompt_width += PROMPT_GROVE.width() + 1 + schema_name.width() + 1;

        match self.spec {
            ConnectionSpec::Sqlite { file, .. } => {
                const PROMPT_SQLITE: &str = "SQLite:";
                let file_name = truncated_file_name(file, 24);
                prompt.push_str(&render::paint(PROMPT_SQLITE, Some("34"), colored));
                prompt.push(' ');
                prompt.push_str(&render::paint(file_name.as_ref(), Some("1;34"), colored));
                prompt.push(' ');
                prompt_width += PROMPT_SQLITE.width() + 1 + file_name.width() + 1;
            }
            ConnectionSpec::Memory => {
                const PROMPT_INMEMORY: &str = "in-memory";
                prompt.push_str(&render::paint(PROMPT_INMEMORY, Some("1;35"), colored));
                prompt.push(' ');
                prompt_width += PROMPT_INMEMORY.width() + 1;
            }
        }

        const PROMPT_MARKER: &str = "> ";
        let prompt_marker = render::paint(PROMPT_MARKER, Some("36"), colored);
        prompt.push_str(&prompt_marker);

        const CONTINUATION_PREFIX: &str = "...";
        let continuation_prefix = render::paint(CONTINUATION_PREFIX, Some("36"), colored);
        let mut continuation = String::with_capacity(
            prompt_width - CONTINUATION_PREFIX.width()
                + continuation_prefix.len()
                + prompt_marker.len(),
        );
        continuation.extend(std::iter::repeat_n(
            ' ',
            prompt_width - CONTINUATION_PREFIX.width(),
        ));
        continuation.push_str(&continuation_prefix);
        continuation.push_str(&prompt_marker);
        (prompt, continuation)
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

    fn report_error(&self, error: CliError) {
        if let Some(message) = error.message {
            eprintln!("error: {message}");
        }
    }
}

fn truncated_file_name(path: &Path, max_width: usize) -> Cow<'_, str> {
    if max_width == 1 {
        return Cow::Borrowed("…");
    }
    let file_name = path
        .file_name()
        .map(OsStr::to_string_lossy)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| Cow::Owned(path.display().to_string()));

    if max_width == 0 || UnicodeWidthStr::width(file_name.as_ref()) <= max_width {
        return file_name;
    }
    let file_name = file_name.into_owned();
    let mut out = String::new();
    let mut width = 0;
    for ch in file_name.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > max_width - 1 {
            break;
        }
        out.push(ch);
        width += char_width;
    }
    out.push('…');
    Cow::Owned(out)
}

fn history_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
        })?;
    Some(base.join("grove").join("history"))
}

#[cfg(test)]
mod tests {
    use grove_exec::SqliteBackend;
    use grove_exec::value::{Scalar, Val};
    use grove_schema::validated::ValidatedSchema;

    use super::*;
    use crate::args::ColorChoice;

    fn settings() -> ReplSettings {
        ReplSettings {
            output: OutputArgs {
                format: Format::Auto,
                color: ColorChoice::Never,
                width: Some(80),
                max_rows: 0,
                max_depth: 6,
                max_cell_width: 40,
                full: false,
                no_headers: false,
                hide_refs: false,
                output: None,
                stats: false,
            },
            stats: false,
        }
    }

    fn spec() -> ConnectionSpec {
        ConnectionSpec::Sqlite {
            file: PathBuf::from("<test>"),
            readonly: false,
        }
    }

    fn session<'a>(
        schema: &'a ValidatedSchema,
        spec: &'a ConnectionSpec,
        backend: &'a SqliteBackend<'a>,
        settings: &'a mut ReplSettings,
        turn: &'a mut usize,
    ) -> ReplSession<'a> {
        ReplSession {
            schema,
            spec,
            schema_path: PathBuf::from("<test>"),
            exec: ExecSession::new(backend, schema),
            types: TypeBindings::new(),
            settings,
            exec_options: ExecOptions {
                pushdown: true,
                materialize_lets: true,
                read_only: false,
                debug_rollback: false,
            },
            turn,
        }
    }

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
    fn prompt_sqlite_backend_aligned_continuation() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let spec = ConnectionSpec::Sqlite {
            file: PathBuf::from("/tmp/db.sqlite3"),
            readonly: false,
        };
        let mut settings = settings();
        let mut turn = 0;

        let session = ReplSession {
            schema: &schema,
            spec: &spec,
            schema_path: PathBuf::from("/tmp/schema.gschema"),
            exec: ExecSession::new(&backend, &schema),
            types: TypeBindings::new(),
            settings: &mut settings,
            exec_options: ExecOptions::default(),
            turn: &mut turn,
        };

        let (full, continuation) = session.prompts();
        assert_eq!(full, "grove schema.gschema SQLite: db.sqlite3 > ");
        assert_eq!(full.find('>'), continuation.find('>'));
    }

    #[test]
    fn prompt_file_names_truncated() {
        let schema = ValidatedSchema::default();
        let backend = SqliteBackend::open_in_memory(&schema).expect("backend");
        let spec = ConnectionSpec::Memory;
        let mut settings = settings();
        let mut turn = 0;
        let long_schema = std::env::temp_dir().join(format!("{}-schema.gschema", "x".repeat(40)));
        let session = ReplSession {
            schema: &schema,
            spec: &spec,
            schema_path: long_schema,
            exec: ExecSession::new(&backend, &schema),
            types: TypeBindings::new(),
            settings: &mut settings,
            exec_options: ExecOptions::default(),
            turn: &mut turn,
        };

        let (full, _) = session.prompts();
        assert!(full.contains("in-memory"), "{full}");
        assert!(full.contains('…'), "{full}");
        assert!(!full.contains(&"x".repeat(40)), "{full}");
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
    fn open_command_accepts_schema() {
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
}
