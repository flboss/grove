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
mod tests {
    use crate::ast::{Expr, TypeExpr};
    use crate::{ParseOutcome, parse_query, parse_query_repl};
    use grove_types::Diagnostic;

    pub(super) fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    pub(super) fn parse_ok(src: &str) -> Expr {
        let (file, diags) = parse_query(src);
        assert!(diags.is_empty(), "unexpected diags for `{src}`: {diags:?}");
        file.expect("file should parse")
            .into_statements_and_output()
            .1
            .expect("file should have output")
    }

    pub(super) fn cast_type(src: &str) -> TypeExpr {
        match parse_ok(src) {
            Expr::Cast { ty, .. } => ty.value,
            other => panic!("expected cast, got {other:?}"),
        }
    }

    #[test]
    fn empty_valid_without_output() {
        let (file, diags) = parse_query("");
        let file = file.expect("empty file should parse");
        assert!(file.statements.is_empty());
        assert!(file.output().is_none());
        assert!(diags.is_empty());
    }

    #[test]
    fn comment_valid_without_output() {
        let (file, diags) = parse_query("// nothing here");
        let file = file.expect("comment-only file should parse");
        assert!(file.statements.is_empty());
        assert!(file.output().is_none());
        assert!(diags.is_empty());
    }

    #[test]
    fn repl_parse_complete() {
        for src in ["let x = 1;", "users", "users;", "let x = 1; x"] {
            assert!(
                matches!(parse_query_repl(src), ParseOutcome::Complete { .. }),
                "expected complete parse for `{src}`"
            );
        }
    }

    #[test]
    fn repl_parse_incomplete() {
        for src in [
            "users[",
            "1 +",
            "let x = 1",
            "if true {",
            "\"unclosed",
            "/* unclosed",
        ] {
            assert!(
                matches!(parse_query_repl(src), ParseOutcome::Incomplete { .. }),
                "expected incomplete parse for `{src}`"
            );
        }
    }

    #[test]
    fn repl_parse_invalid() {
        for src in [";", "let x 1;", "42 \"leftover\""] {
            assert!(
                matches!(parse_query_repl(src), ParseOutcome::Invalid { .. }),
                "expected invalid parse for `{src}`"
            );
        }
    }

    #[test]
    fn error_recovery_projection_items() {
        let (file, diags) = parse_query("users { orders { total }, 42 }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0022", "QP0002"]);
    }

    #[test]
    fn error_recovery_synchronization_semicolon() {
        let src = "users.sort(bad; 42";
        let (file, diags) = parse_query(src);
        assert!(file.is_none());
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == grove_types::Severity::Error)
            .collect();
        assert!(!errors.is_empty());
    }
}
