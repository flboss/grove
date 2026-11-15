use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use chrono::Utc;
use clap::Parser;

use crate::args::{Cli, Command, ConnectionSpec, QueryArgs, QueryInputArgs, ReplArgs};

mod args;
mod diagnostics;
mod render;

fn main() {
    let cli = Cli::parse();
    let code = match cli.command {
        Some(Command::Query(args)) => run_query(args),
        Some(Command::Repl(args)) => run_repl(args),
        None => run_implicit_repl(cli.schema),
    };
    std::process::exit(code);
}

fn run_query(args: QueryArgs) -> i32 {
    let spec = match args.backend.resolve() {
        Ok(spec) => spec,
        Err(message) => return fail(2, message),
    };

    let (schema_name, schema_source) = match read_schema(&args.schema) {
        Ok(source) => source,
        Err((code, message)) => return fail(code, message),
    };

    let Some(validated) = compile_schema(&schema_name, &schema_source) else {
        return 3;
    };

    let (query_name, query_source) = match read_query_input(&args.input) {
        Ok(source) => source,
        Err((code, message)) => return fail(code, message),
    };

    let Some(typed) = compile_query(&query_name, &query_source, &validated) else {
        return 3;
    };

    let conn = match open_connection(&spec) {
        Ok(conn) => conn,
        Err(message) => return fail(1, message),
    };
    let backend = match grove_exec::SqliteBackend::new(conn, &validated) {
        Ok(backend) => backend,
        Err(err) => return fail(1, format!("failed to open storage: {err}")),
    };

    let options = grove_exec::ExecOptions {
        pushdown: !args.no_pushdown,
    };
    match grove_exec::exec_file_with_options(&typed, &backend, &validated, Utc::now(), options) {
        Ok(Some(value)) => {
            let text = match render::render(&value, &validated, args.output.format) {
                Ok(text) => text,
                Err(message) => return fail(1, message),
            };
            if let Err(message) = write_output(args.output.output.as_deref(), &text) {
                return fail(1, message);
            }
        }
        Ok(None) => {}
        Err(grove_exec::ExecError::Abort { message, span }) => {
            let diagnostic = diagnostics::abort(message, span);
            diagnostics::print(
                &query_source,
                &query_name,
                std::slice::from_ref(&diagnostic),
            );
            return 1;
        }
        Err(err) => return fail(1, err.to_string()),
    }

    0
}

fn run_repl(args: ReplArgs) -> i32 {
    if let Err(message) = args.backend.resolve() {
        return fail(2, message);
    }
    repl_not_implemented(&args.schema)
}

fn run_implicit_repl(schema: Option<PathBuf>) -> i32 {
    match schema {
        Some(schema) => repl_not_implemented(&schema),
        None => fail(2, "specify a schema or a subcommand"),
    }
}

fn repl_not_implemented(schema: &Path) -> i32 {
    fail(
        2,
        format!(
            "interactive mode is not yet implemented (schema: {})",
            schema.display()
        ),
    )
}

fn compile_schema(name: &str, source: &str) -> Option<grove_schema::ValidatedSchema> {
    let (schema, diagnostics) = grove_schema::parse_schema(source);
    diagnostics::print(source, name, &diagnostics);
    let schema = schema?;

    let (validated, diagnostics) = grove_schema::validate(schema);
    diagnostics::print(source, name, &diagnostics);
    validated
}

fn compile_query(
    name: &str,
    source: &str,
    schema: &grove_schema::ValidatedSchema,
) -> Option<grove_query::TypedQueryFile> {
    let (file, diagnostics) = grove_query::parse_query(source);
    diagnostics::print(source, name, &diagnostics);
    let file = file?;

    let (typed, diagnostics) = grove_query::typecheck(file, schema);
    diagnostics::print(source, name, &diagnostics);
    typed
}

fn read_schema(path: &Path) -> Result<(String, String), (i32, String)> {
    if !path.is_file() {
        return Err((1, format!("schema file not found: {}", path.display())));
    }
    let source = fs::read_to_string(path)
        .map_err(|err| (1, format!("error reading schema {}: {err}", path.display())))?;
    Ok((path.display().to_string(), source))
}

fn read_query_input(args: &QueryInputArgs) -> Result<(String, String), (i32, String)> {
    if let Some(path) = &args.query_file {
        let source = fs::read_to_string(path)
            .map_err(|err| (1, format!("error reading query {}: {err}", path.display())))?;
        return Ok((path.display().to_string(), source));
    }

    if let Some(query) = &args.query {
        if query == "-" {
            return Ok(("<stdin>".to_string(), read_stdin()?));
        }
        return Ok(("<query>".to_string(), query.clone()));
    }

    if io::stdin().is_terminal() {
        return Err((
            2,
            "no query given; pass a query, `-` for stdin, or --query-file".to_string(),
        ));
    }

    Ok(("<stdin>".to_string(), read_stdin()?))
}

fn read_stdin() -> Result<String, (i32, String)> {
    let mut source = String::new();
    io::stdin()
        .read_to_string(&mut source)
        .map_err(|err| (1, format!("error reading stdin: {err}")))?;
    Ok(source)
}

fn open_connection(spec: &ConnectionSpec) -> Result<rusqlite::Connection, String> {
    match spec {
        ConnectionSpec::Sqlite { file, readonly } => {
            let result = if *readonly {
                rusqlite::Connection::open_with_flags(
                    file,
                    rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
                )
            } else {
                rusqlite::Connection::open(file)
            };
            result.map_err(|err| format!("error opening database {}: {err}", file.display()))
        }
    }
}

fn write_output(path: Option<&Path>, text: &str) -> Result<(), String> {
    if let Some(path) = path {
        fs::write(path, format!("{text}\n"))
            .map_err(|err| format!("error writing {}: {err}", path.display()))
    } else {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{text}").map_err(|err| format!("error writing stdout: {err}"))
    }
}

fn fail(code: i32, message: impl Into<String>) -> i32 {
    eprintln!("error: {}", message.into());
    code
}
