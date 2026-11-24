pub mod ast;
mod error;
pub mod lex;
mod parse;
pub mod token;
pub mod validated;
mod validation;
mod validation_error;

use grove_types::Diagnostic;

use ast::Schema;
pub use validated::ValidatedSchema;

pub fn parse_schema(source: &str) -> (Option<Schema>, Vec<Diagnostic>) {
    let parser = parse::Parser::new(source);
    parser.parse_schema()
}

pub use validation::validate;
