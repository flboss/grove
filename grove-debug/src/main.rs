use std::path::PathBuf;

use ariadne::{Color, Config, IndexType, Label, Report, ReportKind, Source};
use clap::{Parser, Subcommand};

use grove_query::typecheck::typecheck;
use grove_types::Diagnostic;

#[derive(Parser)]
#[command(name = "grove-debug")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Tokenize a schema file and display the token stream.
    SchemaLex {
        /// Path to the schema file, or the schema source itself
        path: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },

    /// Parse a schema file and display the AST.
    SchemaParse {
        /// Path to the schema file, or the schema source itself
        path: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },

    /// Validate a schema file and display the validated model.
    SchemaValidate {
        /// Path to the schema file, or the schema source itself
        path: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },

    /// Tokenize a query file and display the token stream.
    QueryLex {
        /// Path to the query file, or the query source itself
        path: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },

    /// Parse a query file and display the AST.
    QueryParse {
        /// Path to the query file, or the query source itself
        path: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },

    /// Type-check a query file against a schema and display the typed AST.
    QueryTypecheck {
        /// Path to the query file, or the query source itself
        query: PathBuf,

        /// Path to the schema file, or the schema source itself
        schema: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },

    /// Run a query file against a SQLite database and print the result.
    ExecRun {
        /// Path to the query file, or the query source itself
        query: PathBuf,

        /// Path to the schema file, or the schema source itself
        schema: PathBuf,

        /// Path to the SQLite database file
        db: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::SchemaLex { path, raw } => schema_lex(&path, raw),
        Command::SchemaParse { path, raw } => schema_parse(&path, raw),
        Command::SchemaValidate { path, raw } => schema_validate(&path, raw),
        Command::QueryLex { path, raw } => query_lex(&path, raw),
        Command::QueryParse { path, raw } => query_parse(&path, raw),
        Command::QueryTypecheck { query, schema, raw } => query_typecheck(&query, &schema, raw),
        Command::ExecRun {
            query,
            schema,
            db,
            raw,
        } => exec_run(&query, &schema, &db, raw),
    }
}

fn read_input(path: &PathBuf, kind: &str) -> (String, String) {
    if path.is_file() {
        match std::fs::read_to_string(path) {
            Ok(source) => (path.display().to_string(), source),
            Err(e) => {
                eprintln!("error reading {kind} file {}: {e}", path.display());
                std::process::exit(1);
            }
        }
    } else {
        ("<inline>".to_string(), path.to_string_lossy().into_owned())
    }
}

fn schema_lex(path: &PathBuf, raw: bool) {
    let (name, source) = read_input(path, "schema");

    let mut lexer = grove_schema::lex::Lexer::new(&source);
    let mut tokens = Vec::new();
    loop {
        let tok = lexer.next_token();
        let is_eof = matches!(tok.value, grove_schema::token::TokenKind::Eof);
        tokens.push(tok);
        if is_eof {
            break;
        }
    }
    let diagnostics = lexer.take_diagnostics();

    println!("=== Tokens ===");
    for tok in &tokens {
        let kind = format!("{:?}", tok.value);
        let span = &tok.span;
        let text: String = source[span.start..span.end].chars().collect();
        println!(
            "  [{:>4}..{:<4}) {:30} {:?}",
            span.start, span.end, kind, text
        );
    }

    print_diagnostics(&source, &name, &diagnostics, raw, "");
}

fn query_lex(path: &PathBuf, raw: bool) {
    let (name, source) = read_input(path, "query");

    let mut lexer = grove_query::lex::Lexer::new(&source);
    let mut tokens = Vec::new();
    loop {
        let tok = lexer.next_token();
        let is_eof = matches!(tok.value, grove_query::token::TokenKind::Eof);
        tokens.push(tok);
        if is_eof {
            break;
        }
    }
    let diagnostics = lexer.take_diagnostics();

    println!("=== Tokens ===");
    for tok in &tokens {
        let kind = format!("{:?}", tok.value);
        let span = &tok.span;
        let text: String = source[span.start..span.end].chars().collect();
        println!(
            "  [{:>4}..{:<4}) {:30} {:?}",
            span.start, span.end, kind, text
        );
    }

    print_diagnostics(&source, &name, &diagnostics, raw, "");
}

fn query_parse(path: &PathBuf, raw: bool) {
    let (name, source) = read_input(path, "query");

    let (file, diagnostics) = grove_query::parse_query(&source);

    println!("=== AST ===");
    match &file {
        Some(file) => println!("{file:#?}"),
        None => println!("No AST. (parse failed)"),
    }

    print_diagnostics(&source, &name, &diagnostics, raw, "");
}

fn query_typecheck(query_path: &PathBuf, schema_path: &PathBuf, raw: bool) {
    let (query_name, query_source) = read_input(query_path, "query");
    let (schema_name, schema_source) = read_input(schema_path, "schema");

    let (schema, schema_parse_diagnostics) = grove_schema::parse_schema(&schema_source);
    let Some(schema) = schema else {
        println!("No validated schema. (parse failed)");
        print_diagnostics(
            &schema_source,
            &schema_name,
            &schema_parse_diagnostics,
            raw,
            "Schema Parse ",
        );
        return;
    };

    let (validated_schema, schema_validation_diagnostics) = grove_schema::validate(schema);
    let Some(validated_schema) = validated_schema else {
        println!("No validated schema. (validation failed)");
        print_diagnostics(
            &schema_source,
            &schema_name,
            &schema_validation_diagnostics,
            raw,
            "Schema Validation ",
        );
        return;
    };

    let (file, parse_diagnostics) = grove_query::parse_query(&query_source);
    let Some(file) = file else {
        println!("No query AST. (parse failed)");
        print_diagnostics(
            &query_source,
            &query_name,
            &parse_diagnostics,
            raw,
            "Query Parse ",
        );
        return;
    };

    let (typed_file, typecheck_diagnostics) = typecheck(file, &validated_schema);

    println!("=== Typed AST ===");
    match &typed_file {
        Some(typed) => println!("{typed:#?}"),
        None => println!("No typed AST. (typecheck failed)"),
    }

    print_diagnostics(&query_source, &query_name, &typecheck_diagnostics, raw, "");
}

fn schema_parse(path: &PathBuf, raw: bool) {
    let (name, source) = read_input(path, "schema");

    let (schema, diagnostics) = grove_schema::parse_schema(&source);

    println!("=== AST ===");
    match &schema {
        Some(schema) => println!("{schema:#?}"),
        None => println!("No AST. (parse failed)"),
    }

    print_diagnostics(&source, &name, &diagnostics, raw, "");
}

fn schema_validate(path: &PathBuf, raw: bool) {
    let (name, source) = read_input(path, "schema");

    let (schema, parse_diagnostics) = grove_schema::parse_schema(&source);
    let Some(schema) = schema else {
        println!("No validated schema. (parse failed)");
        print_diagnostics(&source, &name, &parse_diagnostics, raw, "");
        return;
    };

    let (validated, validation_diagnostics) = grove_schema::validate(schema);

    println!("=== ValidatedSchema ===");
    match &validated {
        Some(schema) => println!("{schema:#?}"),
        None => println!("No validated schema. (validation failed)"),
    }

    print_diagnostics(&source, &name, &validation_diagnostics, raw, "");
}

fn print_diagnostics(
    source: &str,
    name: &str,
    diagnostics: &[Diagnostic],
    raw: bool,
    specifier: &str,
) {
    if diagnostics.is_empty() {
        println!("\nNo diagnostics.");
        return;
    }

    println!(
        "\n=== {specifier}Diagnostics ({} total) ===\n",
        diagnostics.len()
    );
    if raw {
        for (i, diag) in diagnostics.iter().enumerate() {
            println!("--- {i} ---");
            println!("{diag:#?}");
        }
        return;
    }

    let path_str = name;
    let ariadne_source = Source::from(source);
    let mut cache = (path_str, ariadne_source);

    for diag in diagnostics {
        let location = diag.labels.first().map(|l| l.span.range()).unwrap_or(0..0);
        let mut report = Report::build(
            match diag.severity {
                grove_types::Severity::Error => ReportKind::Error,
                grove_types::Severity::Warning => ReportKind::Warning,
            },
            (path_str, location),
        )
        .with_config(Config::default().with_index_type(IndexType::Byte))
        .with_code(diag.code.as_ref())
        .with_message(diag.message.as_ref());

        for label in &diag.labels {
            let color = match label.style {
                grove_types::LabelStyle::Primary => Color::Red,
                grove_types::LabelStyle::Secondary => Color::Blue,
            };
            report = report.with_label(
                Label::new((path_str, label.span.range()))
                    .with_message(label.message.as_ref())
                    .with_color(color),
            );
        }

        for note in &diag.notes {
            report = report.with_note(note.as_ref());
        }

        for help in &diag.help {
            report = report.with_help(help.as_ref());
        }

        report.finish().print(&mut cache).unwrap();
    }
}

fn exec_run(query_path: &PathBuf, schema_path: &PathBuf, db_path: &PathBuf, raw: bool) {
    let (query_name, query_source) = read_input(query_path, "query");
    let (schema_name, schema_source) = read_input(schema_path, "schema");

    let (schema, schema_parse_diagnostics) = grove_schema::parse_schema(&schema_source);
    let Some(schema) = schema else {
        println!("No validated schema. (parse failed)");
        print_diagnostics(
            &schema_source,
            &schema_name,
            &schema_parse_diagnostics,
            raw,
            "Schema Parse ",
        );
        return;
    };

    let (validated_schema, schema_validation_diagnostics) = grove_schema::validate(schema);
    let Some(validated_schema) = validated_schema else {
        println!("No validated schema. (validation failed)");
        print_diagnostics(
            &schema_source,
            &schema_name,
            &schema_validation_diagnostics,
            raw,
            "Schema Validation ",
        );
        return;
    };

    let (file, parse_diagnostics) = grove_query::parse_query(&query_source);
    let Some(file) = file else {
        println!("No query AST. (parse failed)");
        print_diagnostics(
            &query_source,
            &query_name,
            &parse_diagnostics,
            raw,
            "Query Parse ",
        );
        return;
    };

    let (typed_file, mut typecheck_diagnostics) = typecheck(file, &validated_schema);
    let has_errors = typecheck_diagnostics
        .iter()
        .any(|d| d.severity == grove_types::Severity::Error);
    let Some(typed_file) = typed_file else {
        println!("No typed AST. (typecheck failed)");
        print_diagnostics(&query_source, &query_name, &typecheck_diagnostics, raw, "");
        return;
    };
    if has_errors {
        println!("No result. (typecheck failed)");
        print_diagnostics(&query_source, &query_name, &typecheck_diagnostics, raw, "");
        return;
    }
    typecheck_diagnostics.extend(parse_diagnostics);

    let conn = match rusqlite::Connection::open(db_path) {
        Ok(conn) => conn,
        Err(e) => {
            eprintln!("error opening database {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };
    let backend = match grove_exec::sqlite::SqliteBackend::new(conn, &validated_schema) {
        Ok(backend) => backend,
        Err(e) => {
            println!("No result. (storage failed: {e})");
            return;
        }
    };
    match grove_exec::eval::exec_file(&typed_file, &backend, &validated_schema, chrono::Utc::now())
    {
        Ok(value) => println!("{value}"),
        Err(e) => println!("No result. (execution failed: {e})"),
    }
    print_diagnostics(&query_source, &query_name, &typecheck_diagnostics, raw, "");
}
