mod config;
mod rel;
mod root;
mod r#struct;

use crate::ast::Schema;
use crate::error::SchemaParseError;
use crate::lex::Lexer;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Span, Spanned};

pub type PResult<T> = Result<T, ()>;

pub struct Parser<'src> {
    lexer: Lexer<'src>,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Parser<'src> {
    pub fn new(source: &'src str) -> Self {
        Parser {
            lexer: Lexer::new(source),
            diagnostics: Vec::new(),
        }
    }

    pub fn parse_schema(mut self) -> (Option<Schema>, Vec<Diagnostic>) {
        let mut schema = Schema {
            roots: Vec::new(),
            structs: Vec::new(),
            config: None,
            relations: Vec::new(),
        };
        let mut config_span = None;
        let mut root_spans: Vec<(String, Span)> = Vec::new();
        let mut struct_spans: Vec<(String, Span)> = Vec::new();

        loop {
            let Spanned { span, value: token } = self.advance();
            match token {
                TokenKind::Eof => break,
                TokenKind::Config => {
                    let Ok(mut block) = self.parse_config() else {
                        continue;
                    };

                    block.span.start = span.start;

                    match config_span {
                        Some(previous) => self.emit_error(SchemaParseError::DuplicateConfigBlock {
                            span: block.span,
                            previous,
                        }),
                        None => {
                            config_span = Some(block.span);
                            schema.config = Some(block);
                        }
                    }
                }
                TokenKind::Root => {
                    let Ok(mut root) = self.parse_root() else {
                        continue;
                    };

                    root.span.start = span.start;

                    let name = root.name.value.clone();
                    match root_spans.iter().find(|(n, _)| n == &name) {
                        Some((_, previous)) => self.emit_error(SchemaParseError::DuplicateRoot {
                            span: root.span,
                            name,
                            previous: *previous,
                        }),
                        None => {
                            root_spans.push((name, root.span));
                            schema.roots.push(root.value);
                        }
                    }
                }
                TokenKind::Struct => {
                    let Ok(mut struct_def) = self.parse_struct() else {
                        continue;
                    };

                    struct_def.span.start = span.start;

                    let name = struct_def.name.value.clone();
                    match struct_spans.iter().find(|(n, _)| n == &name) {
                        Some((_, previous)) => {
                            self.emit_error(SchemaParseError::DuplicateStruct {
                                span: struct_def.span,
                                name,
                                previous: *previous,
                            });
                        }
                        None => {
                            struct_spans.push((name, struct_def.span));
                            schema.structs.push(struct_def.value);
                        }
                    }
                }
                TokenKind::Rel => {
                    if let Ok(relation) = self.parse_rel() {
                        schema.relations.push(relation);
                    }
                }
                _ => {
                    self.emit_error(SchemaParseError::ExpectedTopLevelStmt { span });
                    self.sync_to_statement_boundary();
                }
            }
        }

        let mut diagnostics = self.lexer.finalize();
        diagnostics.extend(self.diagnostics);

        (diagnostics.is_empty().then_some(schema), diagnostics)
    }

    fn emit_error(&mut self, error: SchemaParseError) {
        self.diagnostics.push(error.into())
    }

    fn sync_to(&mut self, targets: &[TokenKind]) {
        loop {
            let tok = self.peek();
            if matches!(tok.value, TokenKind::Eof) || targets.contains(&tok.value) {
                break;
            }
            self.advance();
        }
    }

    fn sync_to_statement_boundary(&mut self) {
        self.sync_to(&[
            TokenKind::Semicolon,
            TokenKind::Config,
            TokenKind::Root,
            TokenKind::Struct,
            TokenKind::Rel,
            TokenKind::Eof,
        ]);
        if matches!(self.peek().value, TokenKind::Semicolon) {
            self.advance();
        }
    }

    fn expect(&mut self, kind: TokenKind) -> Result<Token, Token> {
        let token = self.advance();
        if token.value == kind {
            Ok(token)
        } else {
            Err(token)
        }
    }

    fn expect_or_sync(
        &mut self,
        kind: TokenKind,
        err: impl FnOnce(Span) -> SchemaParseError,
    ) -> PResult<Token> {
        let span = self.peek().span;
        if self.peek().value == kind {
            Ok(self.advance())
        } else {
            self.emit_error(err(span));
            self.sync_to_statement_boundary();
            Err(())
        }
    }

    fn expect_ident(&mut self) -> Result<Spanned<String>, Token> {
        let token = self.advance();
        match token.value {
            TokenKind::Ident(s) => Ok(Spanned {
                span: token.span,
                value: s,
            }),
            _ => Err(token),
        }
    }

    fn expect_ident_or_sync(
        &mut self,
        err: impl FnOnce(Span) -> SchemaParseError,
    ) -> PResult<Spanned<String>> {
        let span = self.peek().span;
        match self.peek().value {
            TokenKind::Ident(_) => {
                let ident = match self.advance().value {
                    TokenKind::Ident(s) => s,
                    _ => unreachable!(),
                };
                Ok(Spanned { span, value: ident })
            }
            _ => {
                self.emit_error(err(span));
                self.sync_to_statement_boundary();
                Err(())
            }
        }
    }

    fn advance(&mut self) -> Token {
        self.lexer.next_token()
    }

    fn peek(&mut self) -> &Token {
        self.lexer.peek()
    }
}

fn token_text(tok: &Token) -> String {
    match &tok.value {
        TokenKind::Ident(s) | TokenKind::StringLit(s) => s.clone(),
        other => format!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    #[test]
    fn unknown_top_level() {
        let (_, diags) = parse_schema("foo bar; baz");
        assert_eq!(codes(&diags), vec!["SP0001", "SP0001"]);
    }

    #[test]
    fn duplicate_config_block() {
        let (_, diags) = parse_schema("config { } config { }");
        assert_eq!(codes(&diags), vec!["SP0011"]);
        assert_eq!(diags[0].labels[0].span, Span { start: 11, end: 21 });
        assert_eq!(diags[0].labels[1].span, Span { start: 0, end: 10 });
    }

    #[test]
    fn duplicate_root() {
        let (_, diags) = parse_schema("root users: U; root users: V;");
        assert_eq!(codes(&diags), vec!["SP0017"]);
        assert_eq!(diags[0].labels.len(), 2);
        assert_eq!(diags[0].labels[0].span, Span { start: 15, end: 29 });
        assert_eq!(diags[0].labels[1].span, Span { start: 0, end: 14 });
    }
}
