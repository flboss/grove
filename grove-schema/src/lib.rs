pub mod ast;
pub mod error;
pub mod lex;
pub mod parse;
pub mod token;
pub mod validated;
pub mod validation;
pub mod validation_error;

use grove_types::Diagnostic;

use ast::Schema;

pub fn parse_schema(source: &str) -> (Option<Schema>, Vec<Diagnostic>) {
    let parser = parse::Parser::new(source);
    parser.parse_schema()
}

pub use validation::validate;
