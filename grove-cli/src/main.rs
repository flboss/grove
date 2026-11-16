use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::Utc;
use clap::Parser;

use crate::args::{
    BackendArgs, CheckArgs, Cli, Command, ConnectionSpec, OutputArgs, QueryArgs, QueryInputArgs,
    ReplArgs,
};
use crate::error::{CliError, CliResult, EXIT_COMPILE, EXIT_FAILURE, EXIT_SUCCESS};

mod args;
mod diagnostics;
mod error;
mod render;
mod repl;

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Some(Command::Query(args)) => run_query(args),
        Some(Command::Check(args)) => run_check(args),
        Some(Command::Repl(args)) => repl::run(args),
        None => run_implicit_repl(cli.schema, cli.backend, cli.output),
    };

    match result {
        Ok(()) => std::process::exit(EXIT_SUCCESS),
        Err(error) => {
            if let Some(message) = error.message {
                eprintln!("error: {message}");
            }
            std::process::exit(error.code);
        }
    }
}

fn run_query(args: QueryArgs) -> CliResult<()> {
    let spec = args.backend.resolve().map_err(CliError::usage)?;
    let (schema_name, schema_source) = read_schema(&args.schema)?;

    let schema_start = Instant::now();
    let Some(validated) = compile_schema(&schema_name, &schema_source) else {
        return Err(CliError::reported(EXIT_COMPILE));
    };
    let schema_elapsed = schema_start.elapsed();

    let (query_name, query_source) = read_query_input(&args.input)?;
    let compile_start = Instant::now();
    let Some(typed) = compile_query(&query_name, &query_source, &validated) else {
        return Err(CliError::reported(EXIT_COMPILE));
    };
    let compile_elapsed = compile_start.elapsed();

    let conn = open_connection(&spec)?;
    let backend = grove_exec::SqliteBackend::new(conn, &validated)
        .map_err(|err| CliError::runtime(format!("failed to open storage: {err}")))?;

    let options = grove_exec::ExecOptions {
        pushdown: !args.no_pushdown,
    };
    let render_options =
        render::RenderOptions::from_args(&args.output, args.output.color_enabled());
    let execute_start = Instant::now();
    match grove_exec::exec_file_with_options(&typed, &backend, &validated, Utc::now(), options) {
        Ok(Some(value)) => {
            let text = render::render(&value, typed.output_type(), &validated, &render_options)?;
            write_output(args.output.output.as_deref(), &text)?;
        }
        Ok(None) => {}
        Err(grove_exec::ExecError::Abort { message, span }) => {
            let diagnostic = diagnostics::abort(message, span);
            diagnostics::print(
                &query_source,
                &query_name,
                std::slice::from_ref(&diagnostic),
            );
            return Err(CliError::reported(EXIT_FAILURE));
        }
        Err(err) => return Err(CliError::runtime(err.to_string())),
    }
    let execute_elapsed = execute_start.elapsed();

    if args.output.stats {
        eprintln!(
            "schema: {:.3} ms, compile: {:.3} ms, execute: {:.3} ms",
            millis(schema_elapsed),
            millis(compile_elapsed),
            millis(execute_elapsed),
        );
    }

    Ok(())
}

fn run_check(args: CheckArgs) -> CliResult<()> {
    let (schema_name, schema_source) = read_schema(&args.schema)?;
    let Some(validated) = compile_schema(&schema_name, &schema_source) else {
        return Err(CliError::reported(EXIT_COMPILE));
    };

    let (query_name, query_source) = read_query_input(&args.input)?;
    if compile_query(&query_name, &query_source, &validated).is_none() {
        Err(CliError::reported(EXIT_COMPILE))
    } else {
        Ok(())
    }
}

fn run_implicit_repl(
    schema: Option<PathBuf>,
    backend: BackendArgs,
    output: OutputArgs,
) -> CliResult<()> {
    match schema {
        Some(schema) => repl::run(ReplArgs {
            schema,
            backend,
            output,
            no_pushdown: false,
        }),
        None => Err(CliError::usage("specify a schema or a subcommand")),
    }
}

pub(crate) fn compile_schema(name: &str, source: &str) -> Option<grove_schema::ValidatedSchema> {
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

pub(crate) fn read_schema(path: &Path) -> CliResult<(String, String)> {
    if !path.is_file() {
        return Err(CliError::runtime(format!(
            "schema file not found: {}",
            path.display()
        )));
    }
    let source = fs::read_to_string(path).map_err(|err| {
        CliError::runtime(format!("error reading schema {}: {err}", path.display()))
    })?;
    Ok((path.display().to_string(), source))
}

fn read_query_input(args: &QueryInputArgs) -> CliResult<(String, String)> {
    if let Some(path) = &args.query_file {
        let source = fs::read_to_string(path).map_err(|err| {
            CliError::runtime(format!("error reading query {}: {err}", path.display()))
        })?;
        return Ok((path.display().to_string(), source));
    }

    if let Some(query) = &args.query {
        if query == "-" {
            return Ok(("<stdin>".to_string(), read_stdin()?));
        }
        return Ok(("<query>".to_string(), query.clone()));
    }

    if io::stdin().is_terminal() {
        return Err(CliError::usage(
            "no query given; pass a query, `-` for stdin, or --query-file",
        ));
    }

    Ok(("<stdin>".to_string(), read_stdin()?))
}

fn read_stdin() -> CliResult<String> {
    let mut source = String::new();
    io::stdin()
        .read_to_string(&mut source)
        .map_err(|err| CliError::runtime(format!("error reading stdin: {err}")))?;
    Ok(source)
}

pub(crate) fn open_connection(spec: &ConnectionSpec) -> CliResult<rusqlite::Connection> {
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
            result.map_err(|err| {
                CliError::runtime(format!("error opening database {}: {err}", file.display()))
            })
        }
    }
}

pub(crate) fn write_output(path: Option<&Path>, text: &str) -> CliResult<()> {
    if let Some(path) = path {
        let mut file = fs::File::create(path)
            .map_err(|err| CliError::runtime(format!("error writing {}: {err}", path.display())))?;
        writeln!(file, "{text}")
            .map_err(|err| CliError::runtime(format!("error writing {}: {err}", path.display())))
    } else {
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{text}")
            .map_err(|err| CliError::runtime(format!("error writing stdout: {err}")))
    }
}

pub(crate) fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
