use std::fs;
use std::path::{Path, PathBuf};

use grove_exec::{ExecOptions, ExecSession};
use grove_query::TypeBindings;
use grove_schema::ValidatedSchema;

use crate::args::{ConnectionSpec, OutputArgs, ReplArgs};
use crate::error::{CliError, CliResult, EXIT_COMPILE};

mod execute;
mod meta;
mod session;
#[cfg(test)]
mod tests;

use self::session::history_path;

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
