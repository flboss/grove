use std::path::{Path, PathBuf};

use ariadne::{Color, Label, Report, ReportKind, Source};
use clap::{Parser, Subcommand};

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
        /// Path to the schema file
        path: PathBuf,

        /// Display diagnostics as raw Debug output instead of using ariadne
        #[arg(long)]
        raw: bool,
    },

    /// Parse a schema file and display the AST.
    SchemaParse {
        /// Path to the schema file
        path: PathBuf,

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
    }
}

fn schema_lex(path: &PathBuf, raw: bool) {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {}: {e}", path.display());
            std::process::exit(1);
        }
    };

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
    let diagnostics = lexer.finalize();

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

    print_diagnostics(&source, path, &diagnostics, raw);
}

fn schema_parse(path: &PathBuf, raw: bool) {
    let source = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error reading {}: {e}", path.display());
            std::process::exit(1);
        }
    };

    let (schema, diagnostics) = grove_schema::parse_schema(&source);

    println!("=== AST ===");
    match &schema {
        Some(schema) => println!("{schema:#?}"),
        None => println!("No AST. (parse failed)"),
    }

    print_diagnostics(&source, path, &diagnostics, raw);
}

fn print_diagnostics(source: &str, path: &Path, diagnostics: &[Diagnostic], raw: bool) {
    if diagnostics.is_empty() {
        println!("\nNo diagnostics.");
        return;
    }

    println!("\n=== Diagnostics ({} total) ===\n", diagnostics.len());
    if raw {
        for (i, diag) in diagnostics.iter().enumerate() {
            println!("--- {i} ---");
            println!("{diag:#?}");
        }
        return;
    }

    let path_str = path.to_string_lossy();
    let ariadne_source = Source::from(source);
    let mut cache = (path_str.as_ref(), ariadne_source);

    for diag in diagnostics {
        let location = diag.labels.first().map(|l| l.span.range()).unwrap_or(0..0);
        let mut report = Report::build(
            match diag.severity {
                grove_types::Severity::Error => ReportKind::Error,
                grove_types::Severity::Warning => ReportKind::Warning,
            },
            (path_str.as_ref(), location),
        )
        .with_code(diag.code.as_ref())
        .with_message(diag.message.as_ref());

        for label in &diag.labels {
            let color = match label.style {
                grove_types::LabelStyle::Primary => Color::Red,
                grove_types::LabelStyle::Secondary => Color::Blue,
            };
            report = report.with_label(
                Label::new((path_str.as_ref(), label.span.range()))
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
