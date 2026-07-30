use std::path::PathBuf;

use ariadne::{Color, Label, Report, ReportKind, Source};
use clap::{Parser, Subcommand};

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
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::SchemaLex { path, raw } => schema_lex(&path, raw),
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

    if diagnostics.is_empty() {
        println!("\nNo diagnostics.");
    } else {
        println!("\n=== Diagnostics ({} total) ===", diagnostics.len());
        if raw {
            for (i, diag) in diagnostics.iter().enumerate() {
                println!("--- {i} ---");
                println!("{diag:#?}");
            }
        } else {
            let path_str = path.to_string_lossy();
            let ariadne_source = Source::from(&source);
            let mut cache = (path_str.as_ref(), ariadne_source);

            for diag in &diagnostics {
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
                        Label::new((path_str.as_ref(), label.span.start..label.span.end))
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
    }
}
