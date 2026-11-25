pub mod ast;
mod error;
pub mod lex;
mod parse;
pub mod token;
mod validate;
pub mod validated;

use grove_types::Diagnostic;

use ast::Schema;
pub use validated::ValidatedSchema;

pub fn parse_schema(source: &str) -> (Option<Schema>, Vec<Diagnostic>) {
    let parser = parse::Parser::new(source);
    parser.parse_schema()
}

pub use validate::validate;
