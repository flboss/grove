use crate::ast::{Expr, LetStmt, MutationKind, MutationStmt, QueryFile, Statement};
use crate::parse::Parser;
use crate::parse::error::QueryParseError;
use crate::token::TokenKind;
use grove_types::Spanned;

impl<'src> Parser<'src> {
    pub(super) fn parse_file(&mut self) -> Result<QueryFile, ()> {
        let mut statements = Vec::new();

        loop {
            if matches!(self.current.value, TokenKind::Eof) {
                break;
            }

            if matches!(self.current.value, TokenKind::Semicolon) {
                self.emit_error(QueryParseError::EmptyStatement {
                    span: self.current.span,
                });
                self.bump();
                continue;
            }

            if matches!(self.current.value, TokenKind::Let) {
                let Ok((name, value)) = self.parse_let_binding() else {
                    while !matches!(self.current.value, TokenKind::Semicolon | TokenKind::Eof) {
                        self.bump();
                    }
                    if matches!(self.current.value, TokenKind::Semicolon) {
                        self.bump();
                    }
                    continue;
                };

                if matches!(self.current.value, TokenKind::Semicolon) {
                    self.bump();
                    statements.push(Statement::Let(LetStmt { name, value }));
                    continue;
                }

                let span = self.current.span;
                if matches!(self.current.value, TokenKind::Eof) {
                    self.emit_error(QueryParseError::ExpectedSemicolon {
                        span,
                        after: "let binding",
                    });
                    return Err(());
                }

                self.emit_error(QueryParseError::TrailingInput { span });
                return Err(());
            }

            let Ok(expr) = self.parse_expr(true) else {
                while !matches!(self.current.value, TokenKind::Semicolon | TokenKind::Eof) {
                    self.bump();
                }
                if matches!(self.current.value, TokenKind::Semicolon) {
                    self.bump();
                }
                continue;
            };

            if matches!(self.current.value, TokenKind::Semicolon) {
                self.bump();

                if is_mutation(&expr) {
                    statements.push(Statement::Mutation(reclassify_mutation(expr)));
                } else {
                    statements.push(Statement::Expr(expr));
                }
                continue;
            }

            if !matches!(self.current.value, TokenKind::Eof) {
                self.emit_error(QueryParseError::TrailingInput {
                    span: self.current.span,
                });
                return Err(());
            }

            if is_mutation(&expr) {
                self.emit_error(QueryParseError::ExpectedSemicolon {
                    span: self.current.span,
                    after: "mutation statement",
                });
                return Err(());
            }

            statements.push(Statement::Expr(expr));
            break;
        }

        let last = statements.len().saturating_sub(1);
        for (index, statement) in statements.iter().enumerate() {
            if index == last {
                break;
            }
            if let Statement::Expr(expr) = statement {
                self.emit_warning(QueryParseError::DiscardedQuery { span: expr.span() });
            }
        }

        Ok(QueryFile { statements })
    }

    fn parse_let_binding(&mut self) -> Result<(Spanned<String>, Expr), ()> {
        self.bump();
        let name = match &self.current.value {
            TokenKind::Ident(_) => self.expect_ident()?,
            _ => {
                let span = self.current.span;
                self.emit_error(QueryParseError::LetNameExpected { span });
                return Err(());
            }
        };
        self.expect(
            |k| matches!(k, TokenKind::Equals),
            |span| QueryParseError::LetEqualsExpected { span },
        )?;
        let value = self.parse_expr(true)?;
        if is_mutation(&value) {
            self.emit_error(QueryParseError::MutationAsValue { span: value.span() });
            return Err(());
        }
        Ok((name, value))
    }
}

fn is_mutation(expr: &Expr) -> bool {
    matches!(
        expr,
        Expr::Method {
            name,
            ..
        } if matches!(name.value.as_str(), "insert" | "update" | "delete")
    )
}

fn reclassify_mutation(expr: Expr) -> MutationStmt {
    let Expr::Method {
        base,
        name,
        args,
        opt_depth,
        ..
    } = expr
    else {
        unreachable!()
    };

    let kind = match name.value.as_str() {
        "insert" => MutationKind::Insert,
        "update" => MutationKind::Update,
        "delete" => MutationKind::Delete,
        _ => unreachable!(),
    };

    MutationStmt {
        kind: Spanned::new(name.span, kind),
        base: *base,
        args: args.into_iter().map(|arg| arg.expr).collect(),
        opt_depth,
    }
}
