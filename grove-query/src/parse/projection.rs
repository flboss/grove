use crate::ast::{Expr, ProjectionItem};
use crate::parse::Parser;
use crate::parse::error::QueryParseError;
use crate::token::TokenKind;
use grove_types::{Span, Spanned};

impl<'src> Parser<'src> {
    pub(super) fn parse_projection_body(
        &mut self,
    ) -> Result<(Vec<ProjectionItem>, Option<Span>), ()> {
        let mut items = Vec::new();
        let mut spread = None;
        while !matches!(self.current.value, TokenKind::RBrace | TokenKind::Eof) {
            if let Some(span) = spread {
                self.emit_error(QueryParseError::ProjectionSpreadNotLast { span });
                self.sync_to(&[TokenKind::RBrace]);
                break;
            }
            if matches!(self.current.value, TokenKind::TripleDot) {
                spread = Some(self.bump().span);
                if matches!(self.current.value, TokenKind::Comma) {
                    self.bump();
                }
                continue;
            }
            match self.parse_projection_item() {
                Ok(item) => {
                    items.push(item);
                }
                Err(()) => {
                    self.sync_to(&[TokenKind::Comma, TokenKind::RBrace]);
                    if matches!(self.current.value, TokenKind::Comma) {
                        self.bump();
                    }
                    continue;
                }
            }
            if !matches!(self.current.value, TokenKind::Comma) {
                break;
            }
            self.bump();
        }
        Ok((items, spread))
    }

    fn parse_projection_item(&mut self) -> Result<ProjectionItem, ()> {
        if matches!(&self.current.value, TokenKind::Ident(_))
            && matches!(self.lexer.peek().value, TokenKind::Equals)
        {
            let alias = self.expect_ident()?;
            self.bump();
            let value = self.parse_expr(true)?;
            return Ok(ProjectionItem {
                alias: Some(alias),
                value,
            });
        }

        let path = self.parse_pure_path()?;

        if matches!(
            self.current.value,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace
        ) {
            self.emit_error(QueryParseError::ProjectionItemAliasRequired {
                span: self.span_from(path.span().start),
            });
            return Err(());
        }

        Ok(ProjectionItem {
            alias: None,
            value: path,
        })
    }

    fn parse_pure_path(&mut self) -> Result<Expr, ()> {
        let Ok(first) = self.expect_ident() else {
            self.emit_error(QueryParseError::ProjectionItemAliasRequired {
                span: self.current.span,
            });
            return Err(());
        };
        let mut expr = Expr::Ident(first);

        while matches!(
            self.current.value,
            TokenKind::Dot | TokenKind::QuestionDot(_)
        ) {
            let opt_depth = match self.current.value {
                TokenKind::Dot => 0,
                TokenKind::QuestionDot(depth) => depth,
                _ => unreachable!(),
            };
            self.bump();
            if let TokenKind::IntLit(n) = &self.current.value {
                let (n, index_span) = (*n, self.current.span);
                self.bump();
                let span = expr.span().merge(index_span);
                expr = Expr::TupleAccess {
                    base: Box::new(expr),
                    index: Spanned::new(index_span, n),
                    opt_depth,
                    span,
                };
                continue;
            }
            let span = self.span_from(expr.span().start);
            let Ok(name) = self.expect_ident() else {
                self.emit_error(QueryParseError::ExpectedIdentAfterDot {
                    span: self.current.span,
                });
                return Err(());
            };
            expr = Expr::Field {
                base: Box::new(expr),
                name,
                opt_depth,
                span,
            };
        }

        Ok(expr)
    }
}
