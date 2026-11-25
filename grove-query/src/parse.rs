use crate::ast::QueryFile;
use crate::lex::Lexer;
use crate::parse::error::QueryParseError;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Severity, Span, Spanned};

mod error;
mod expr;
mod primary;
mod projection;
mod statements;
mod types;

pub(crate) struct Parser<'src> {
    lexer: Lexer<'src>,
    current: Token,
    diagnostics: Vec<Diagnostic>,
    eof_error: bool,
    non_eof_error: bool,
}

impl<'src> Parser<'src> {
    pub(crate) fn new(source: &'src str) -> Self {
        let mut lexer = Lexer::new(source);
        let current = lexer.next_token();
        let mut parser = Parser {
            lexer,
            current,
            diagnostics: Vec::new(),
            eof_error: false,
            non_eof_error: false,
        };
        parser.drain_lexer_diagnostics();
        parser
    }

    pub(crate) fn parse_query(mut self) -> (Option<QueryFile>, Vec<Diagnostic>) {
        let mut result = self.parse_file().ok();
        self.drain_lexer_diagnostics();
        let mut diagnostics = self.diagnostics;
        diagnostics.extend(self.lexer.take_diagnostics());
        if diagnostics.iter().any(|d| d.severity == Severity::Error) {
            result.take();
        }
        (result, diagnostics)
    }

    pub(crate) fn parse_query_repl(mut self) -> crate::ParseOutcome {
        let file = self.parse_file().ok();
        self.drain_lexer_diagnostics();
        let mut diagnostics = self.diagnostics;
        diagnostics.extend(self.lexer.take_diagnostics());
        let has_error = diagnostics.iter().any(|d| d.severity == Severity::Error);

        match file {
            Some(file) if !has_error => crate::ParseOutcome::Complete { file, diagnostics },
            _ if has_error && self.eof_error && !self.non_eof_error => {
                crate::ParseOutcome::Incomplete { diagnostics }
            }
            _ => crate::ParseOutcome::Invalid { diagnostics },
        }
    }

    fn drain_lexer_diagnostics(&mut self) {
        let diagnostics = self.lexer.take_diagnostics();
        for diagnostic in &diagnostics {
            if diagnostic.severity != Severity::Error {
                continue;
            }
            if matches!(
                diagnostic.code.as_ref(),
                "QL0002" | "QL0003" | "QL0004" | "QL0017" | "QL0040"
            ) {
                self.eof_error = true;
            } else {
                self.non_eof_error = true;
            }
        }
        self.diagnostics.extend(diagnostics);
    }

    fn bump(&mut self) -> Token {
        let tok = std::mem::replace(&mut self.current, self.lexer.next_token());
        self.drain_lexer_diagnostics();
        tok
    }

    fn expect(
        &mut self,
        pred: impl Fn(&TokenKind) -> bool,
        err: impl FnOnce(Span) -> QueryParseError,
    ) -> Result<Token, ()> {
        if pred(&self.current.value) {
            Ok(self.bump())
        } else {
            let span = self.current.span;
            self.emit_error(err(span));
            Err(())
        }
    }

    fn span_from(&self, start: usize) -> Span {
        Span::new(start, self.current.span.end)
    }

    fn sync_to(&mut self, targets: &[TokenKind]) {
        while !targets.contains(&self.current.value)
            && !matches!(self.current.value, TokenKind::Semicolon | TokenKind::Eof)
        {
            self.bump();
        }
    }

    fn expect_ident(&mut self) -> Result<Spanned<String>, ()> {
        if matches!(self.current.value, TokenKind::Ident(_)) {
            Ok(self.bump().map(|t| match t {
                TokenKind::Ident(s) => s,
                _ => unreachable!(),
            }))
        } else {
            Err(())
        }
    }

    fn emit_error(&mut self, err: QueryParseError) {
        if matches!(self.current.value, TokenKind::Eof) {
            self.eof_error = true;
        } else {
            self.non_eof_error = true;
        }
        self.diagnostics.push(err.into());
    }

    fn emit_warning(&mut self, err: QueryParseError) {
        self.diagnostics.push(err.into());
    }
}

#[cfg(test)]
mod tests;
