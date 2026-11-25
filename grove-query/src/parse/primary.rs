use crate::ast::{ConstantName, Expr, Literal};
use crate::parse::Parser;
use crate::parse::error::QueryParseError;
use crate::token::{Token, TokenKind};
use grove_types::{Span, Spanned};

impl<'src> Parser<'src> {
    pub(super) fn parse_primary(&mut self) -> Result<Expr, ()> {
        let span = self.current.span;

        match &self.current.value {
            &TokenKind::IntLit(i) => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Int(i))))
            }
            &TokenKind::FloatLit(f) => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Float(f))))
            }
            &TokenKind::DecLit(d) => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Dec(d))))
            }
            TokenKind::StringLit(_) => {
                let TokenKind::StringLit(s) = self.bump().value else {
                    unreachable!()
                };
                Ok(Expr::Literal(Spanned::new(span, Literal::String(s))))
            }
            TokenKind::BytesLit(_) => {
                let TokenKind::BytesLit(bytes) = self.bump().value else {
                    unreachable!()
                };
                Ok(Expr::Literal(Spanned::new(span, Literal::Bytes(bytes))))
            }
            TokenKind::True => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Bool(true))))
            }
            TokenKind::False => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Bool(false))))
            }
            TokenKind::None => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::None)))
            }
            &TokenKind::InstantLit(dt) => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Instant(dt))))
            }
            TokenKind::Now => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Now)))
            }
            &TokenKind::Today(t) => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Today(t))))
            }
            &TokenKind::DurationLit(d) => {
                self.bump();
                Ok(Expr::Literal(Spanned::new(span, Literal::Duration(d))))
            }
            TokenKind::Ident(_) => {
                let name = self.expect_ident()?;
                if matches!(self.current.value, TokenKind::DoubleColon) {
                    self.bump();
                    let variant = match self.current.value {
                        TokenKind::Ident(_) => self.expect_ident()?,
                        _ => {
                            let span = self.current.span;
                            self.emit_error(QueryParseError::ExpectedEnumVariantName { span });
                            return Err(());
                        }
                    };
                    let span = name.span.merge(variant.span);
                    Ok(Expr::EnumVariant {
                        enum_name: name,
                        variant,
                        span,
                    })
                } else {
                    Ok(Expr::Ident(name))
                }
            }
            TokenKind::Prev => {
                self.bump();
                Ok(Expr::Ident(Spanned::new(span, "prev".to_string())))
            }
            &TokenKind::Dollar(count) => {
                self.bump();
                Ok(Expr::Ident(Spanned::new(span, "$".repeat(count))))
            }
            TokenKind::LParen => self.parse_tuple_or_group(),
            TokenKind::LBracket => self.parse_array(),
            TokenKind::Some => self.parse_some(),
            TokenKind::Int | TokenKind::Float | TokenKind::Dec => self.parse_type_constant(),
            TokenKind::If => self.parse_if(),
            TokenKind::LBrace => self.parse_struct_literal(),
            _ => {
                self.emit_error(QueryParseError::ExpectedExpr { span });
                Err(())
            }
        }
    }

    fn parse_tuple_or_group(&mut self) -> Result<Expr, ()> {
        let lparen = self.bump();

        let first = self.parse_expr(true);

        if first.is_err() {
            self.sync_to(&[TokenKind::Comma, TokenKind::RParen]);
        }

        if !matches!(self.current.value, TokenKind::Comma) {
            self.expect(
                |k| matches!(k, TokenKind::RParen),
                |span| QueryParseError::TupleCommaOrRParenExpected { span },
            )?;
            return first;
        }

        let mut elements: Vec<Expr> = first.into_iter().collect();
        while matches!(self.current.value, TokenKind::Comma) {
            self.bump();
            if matches!(self.current.value, TokenKind::RParen) {
                break;
            }
            match self.parse_expr(true) {
                Ok(expr) => elements.push(expr),
                Err(()) => {
                    self.sync_to(&[TokenKind::Comma, TokenKind::RParen]);
                    if matches!(self.current.value, TokenKind::Comma) {
                        self.bump();
                    }
                    continue;
                }
            }
        }
        self.expect(
            |k| matches!(k, TokenKind::RParen),
            |span| QueryParseError::TupleCommaOrRParenExpected { span },
        )?;

        Ok(Expr::Tuple {
            elements,
            span: self.span_from(lparen.span.start),
        })
    }

    fn parse_array(&mut self) -> Result<Expr, ()> {
        let lbracket = self.bump();
        let mut elements = Vec::new();

        while !matches!(self.current.value, TokenKind::RBracket | TokenKind::Eof) {
            match self.parse_expr(true) {
                Ok(expr) => elements.push(expr),
                Err(()) => {
                    self.sync_to(&[TokenKind::Comma, TokenKind::RBracket]);
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

        self.expect(
            |k| matches!(k, TokenKind::RBracket),
            |span| QueryParseError::ArrayCommaOrRBracketExpected { span },
        )?;
        Ok(Expr::Array {
            elements,
            span: self.span_from(lbracket.span.start),
        })
    }

    fn parse_some(&mut self) -> Result<Expr, ()> {
        let kw = self.bump();
        self.expect(
            |k| matches!(k, TokenKind::LParen),
            |span| QueryParseError::SomeLParenExpected { span },
        )?;
        let value = self.parse_expr(true)?;
        let rparen = self.expect(
            |k| matches!(k, TokenKind::RParen),
            |span| QueryParseError::SomeRParenExpected { span },
        )?;
        Ok(Expr::Some {
            value: Box::new(value),
            span: kw.span.merge(rparen.span),
        })
    }

    fn parse_type_constant(&mut self) -> Result<Expr, ()> {
        // only numeric types are currently dispatched here
        let ty = self
            .parse_type_name()
            .expect("only type keywords dispatch here");

        if !matches!(self.current.value, TokenKind::DoubleColon) {
            self.emit_error(QueryParseError::TypeConstColonExpected { span: ty.span });
            return Err(());
        }
        self.bump();

        let TokenKind::Ident(ref name) = self.current.value else {
            self.emit_error(QueryParseError::TypeConstNameExpected {
                span: self.current.span,
            });
            return Err(());
        };
        let constant = match name.as_str() {
            "MIN" => ConstantName::Min,
            "MAX" => ConstantName::Max,
            _ => {
                self.emit_error(QueryParseError::TypeConstNameExpected {
                    span: self.current.span,
                });
                return Err(());
            }
        };
        self.bump();

        Ok(Expr::TypeConstant {
            ty,
            name: Spanned::new(self.current.span, constant),
        })
    }

    fn parse_if_condition(&mut self) -> Result<Expr, ()> {
        if matches!(self.current.value, TokenKind::LParen) {
            self.bump();
            let cond = self.parse_expr(true)?;
            self.expect(
                |k| matches!(k, TokenKind::RParen),
                |span| QueryParseError::TupleCommaOrRParenExpected { span },
            )?;
            Ok(cond)
        } else {
            self.parse_expr(false)
        }
    }

    fn parse_if(&mut self) -> Result<Expr, ()> {
        let if_kw = self.bump();
        let start = if_kw.span.start;

        let cond = self.parse_if_condition()?;
        self.expect(
            |k| matches!(k, TokenKind::LBrace),
            |span| QueryParseError::IfLBraceExpected { span },
        )?;
        let value = self.parse_expr(true)?;
        let rbrace = self.expect(
            |k| matches!(k, TokenKind::RBrace),
            |span| QueryParseError::IfRBraceExpected { span },
        )?;

        let mut arms = vec![(cond, value)];
        let mut end = rbrace.span.end;
        let mut default: Option<Box<Expr>> = None;

        while matches!(self.current.value, TokenKind::Else) {
            self.bump();
            if matches!(self.current.value, TokenKind::If) {
                self.bump();
                let cond = self.parse_if_condition()?;
                self.expect(
                    |k| matches!(k, TokenKind::LBrace),
                    |span| QueryParseError::IfLBraceExpected { span },
                )?;
                let value = self.parse_expr(true)?;
                let rbrace = self.expect(
                    |k| matches!(k, TokenKind::RBrace),
                    |span| QueryParseError::IfRBraceExpected { span },
                )?;
                arms.push((cond, value));
                end = rbrace.span.end;
            } else {
                self.expect(
                    |k| matches!(k, TokenKind::LBrace),
                    |span| QueryParseError::ElseLBraceExpected { span },
                )?;
                let value = self.parse_expr(true)?;
                let rbrace = self.expect(
                    |k| matches!(k, TokenKind::RBrace),
                    |span| QueryParseError::IfRBraceExpected { span },
                )?;
                default = Some(Box::new(value));
                end = rbrace.span.end;
                break;
            }
        }

        let Some(default) = default else {
            self.emit_error(QueryParseError::MissingElse {
                span: Span::new(start, end),
            });
            return Err(());
        };

        Ok(Expr::If {
            arms,
            default,
            span: Span::new(start, end),
        })
    }

    fn parse_struct_literal(&mut self) -> Result<Expr, ()> {
        let lbrace = self.bump();
        let mut fields = Vec::new();

        loop {
            let Token { span, value } = &self.current;
            let name = match value {
                TokenKind::Ident(n) => Spanned::new(*span, n.clone()),
                _ => {
                    self.emit_error(QueryParseError::StructFieldNameExpected { span: *span });
                    return Err(());
                }
            };
            self.bump();

            self.expect(
                |k| matches!(k, TokenKind::Equals),
                |span| QueryParseError::StructEqualsExpected {
                    span,
                    field: name.value.clone(),
                },
            )?;
            let field_value = self.parse_expr(true)?;
            fields.push((name, field_value));

            if matches!(self.current.value, TokenKind::Comma) {
                self.bump();
                if matches!(self.current.value, TokenKind::RBrace) {
                    break;
                }
                continue;
            }
            break;
        }

        let rbrace = self.expect(
            |k| matches!(k, TokenKind::RBrace),
            |span| QueryParseError::StructCommaOrRBraceExpected { span },
        )?;
        Ok(Expr::Struct {
            fields,
            span: lbrace.span.merge(rbrace.span),
        })
    }
}
