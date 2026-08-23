use crate::ast::{Expr, QueryFile};
use crate::error::QueryParseError;
use crate::lex::Lexer;
use crate::token::Token;
use grove_types::Diagnostic;

pub struct Parser<'src> {
    lexer: Lexer<'src>,
    current: Token,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Parser<'src> {
    pub fn new(source: &'src str) -> Self {
        let mut lexer = Lexer::new(source);
        let current = lexer.next_token();
        let mut parser = Parser {
            lexer,
            current,
            diagnostics: Vec::new(),
        };
        parser.drain_lexer_diagnostics();
        parser
    }

    pub fn parse_query(mut self) -> (Option<QueryFile>, Vec<Diagnostic>) {
        let result = self.parse_file().ok();
        self.drain_lexer_diagnostics();
        let mut diagnostics = self.diagnostics;
        diagnostics.extend(self.lexer.take_diagnostics());
        (result, diagnostics)
    }

    fn drain_lexer_diagnostics(&mut self) {
        self.diagnostics.extend(self.lexer.take_diagnostics());
    }

    fn parse_file(&mut self) -> Result<QueryFile, ()> {
        let result = self.parse_expr()?;
        Ok(QueryFile {
            statements: Vec::new(),
            result,
        })
    }

    fn parse_expr(&mut self) -> Result<Expr, ()> {
        // TODO: primary expression parsing
        self.emit_error(QueryParseError::ExpectedExpr {
            span: self.current.span,
        });
        Err(())
    }

    fn emit_error(&mut self, err: QueryParseError) {
        self.diagnostics.push(err.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_query;
    use grove_types::Diagnostic;

    fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics.iter().map(|d| d.code.as_ref()).collect()
    }

    #[test]
    fn empty_is_missing_result_expression() {
        let (file, diags) = parse_query("");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }

    #[test]
    fn comment_is_missing_result_expression() {
        let (file, diags) = parse_query("// nothing here");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }
}
