pub mod ast;
pub mod casts;
pub mod error;
pub mod lex;
pub mod methods;
pub mod parse;
pub mod token;
pub mod typecheck;

use grove_types::Diagnostic;

use ast::QueryFile;

pub use typecheck::{TypeBindings, TypedQueryFile, typecheck, typecheck_with_bindings};

#[derive(Debug, Clone, PartialEq)]
pub enum ParseOutcome {
    Complete {
        file: QueryFile,
        diagnostics: Vec<Diagnostic>,
    },
    Incomplete {
        diagnostics: Vec<Diagnostic>,
    },
    Invalid {
        diagnostics: Vec<Diagnostic>,
    },
}

pub fn parse_query(source: &str) -> (Option<QueryFile>, Vec<Diagnostic>) {
    let parser = parse::Parser::new(source);
    parser.parse_query()
}

pub fn parse_query_repl(source: &str) -> ParseOutcome {
    let parser = parse::Parser::new(source);
    parser.parse_query_repl()
}
