pub mod ast;
pub mod error;
pub mod lex;
pub mod parse;
pub mod token;
pub mod typecheck;

use grove_types::Diagnostic;

use ast::QueryFile;

pub fn parse_query(source: &str) -> (Option<QueryFile>, Vec<Diagnostic>) {
    let parser = parse::Parser::new(source);
    parser.parse_query()
}
