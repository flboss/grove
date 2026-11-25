use crate::ast::{Arg, BinaryOp, Expr, SortDir, UnaryOp};
use crate::parse::Parser;
use crate::parse::error::QueryParseError;
use crate::token::TokenKind;
use grove_types::Spanned;

impl<'src> Parser<'src> {
    pub(super) fn parse_expr(&mut self, allow_projection_braces: bool) -> Result<Expr, ()> {
        self.parse_binary(0, allow_projection_braces)
    }

    fn parse_binary(&mut self, min_prec: u8, allow_projection_braces: bool) -> Result<Expr, ()> {
        let mut lhs = self.parse_unary(allow_projection_braces)?;
        while let Some((op, prec)) = binary_op(&self.current.value)
            && prec >= min_prec
        {
            let op_tok = self.bump();
            let rhs = self.parse_binary(prec + 1, allow_projection_braces)?;
            let span = self.span_from(lhs.span().start);
            lhs = Expr::Binary {
                op: Spanned::new(op_tok.span, op),
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self, allow_projection_braces: bool) -> Result<Expr, ()> {
        match self.current.value {
            TokenKind::Ampersand => {
                let op_tok = self.bump();
                let expr = self.parse_unary(allow_projection_braces)?;
                let span = self.span_from(op_tok.span.start);
                Ok(Expr::RowRef {
                    expr: Box::new(expr),
                    span,
                })
            }
            TokenKind::Minus => {
                let op_tok = self.bump();
                let expr = self.parse_unary(allow_projection_braces)?;
                let span = self.span_from(op_tok.span.start);
                Ok(Expr::Unary {
                    op: Spanned::new(op_tok.span, UnaryOp::Neg),
                    expr: Box::new(expr),
                    span,
                })
            }
            TokenKind::Bang => {
                let op_tok = self.bump();
                let expr = self.parse_unary(allow_projection_braces)?;
                let span = self.span_from(op_tok.span.start);
                Ok(Expr::Unary {
                    op: Spanned::new(op_tok.span, UnaryOp::Not),
                    expr: Box::new(expr),
                    span,
                })
            }
            _ => self.parse_postfix(allow_projection_braces),
        }
    }

    fn parse_postfix(&mut self, allow_projection_braces: bool) -> Result<Expr, ()> {
        let mut expr = self.parse_primary()?;

        loop {
            match self.current.value {
                TokenKind::Dot | TokenKind::QuestionDot(_) => {
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
                    } else {
                        let Ok(name) = self.expect_ident() else {
                            self.emit_error(QueryParseError::ExpectedIdentAfterDot {
                                span: self.current.span,
                            });
                            return Err(());
                        };
                        if matches!(self.current.value, TokenKind::LParen) {
                            let args = self.parse_method_args()?;
                            let span = self.span_from(expr.span().start);
                            expr = Expr::Method {
                                base: Box::new(expr),
                                name,
                                args,
                                opt_depth,
                                span,
                            };
                        } else {
                            let span = expr.span().merge(name.span);
                            expr = Expr::Field {
                                base: Box::new(expr),
                                name,
                                opt_depth,
                                span,
                            };
                        }
                    }
                }
                TokenKind::LBracket => {
                    let lbracket = self.bump();
                    let cond = self.parse_expr(true)?;
                    self.expect(
                        |k| matches!(k, TokenKind::RBracket),
                        |span| QueryParseError::ExpectedFilterRBracket { span },
                    )?;
                    let filter_name = Spanned::new(lbracket.span, "filter".to_string());
                    let span = self.span_from(expr.span().start);
                    expr = Expr::Method {
                        base: Box::new(expr),
                        name: filter_name,
                        args: vec![Arg {
                            direction: None,
                            expr: cond,
                        }],
                        opt_depth: 0,
                        span,
                    };
                }
                TokenKind::LBrace if allow_projection_braces => {
                    self.bump();
                    let (items, spread) = self.parse_projection_body()?;
                    self.expect(
                        |k| matches!(k, TokenKind::RBrace),
                        |span| QueryParseError::ExpectedProjectionRBrace { span },
                    )?;
                    let span = self.span_from(expr.span().start);
                    expr = Expr::Projection {
                        base: Box::new(expr),
                        items,
                        spread,
                        span,
                    };
                }
                TokenKind::As | TokenKind::AsQuestion => {
                    let fallible = matches!(self.current.value, TokenKind::AsQuestion);
                    self.bump();
                    let ty = self.parse_type_expr()?;
                    let span = expr.span().merge(ty.span);
                    expr = Expr::Cast {
                        expr: Box::new(expr),
                        ty,
                        fallible,
                        span,
                    };
                }
                _ => break,
            }
        }

        Ok(expr)
    }

    fn parse_method_args(&mut self) -> Result<Vec<Arg>, ()> {
        self.bump();
        let mut args = Vec::new();

        while !matches!(self.current.value, TokenKind::RParen | TokenKind::Eof) {
            let direction = match self.current.value {
                TokenKind::Asc => {
                    let span = self.bump().span;
                    Some(Spanned::new(span, SortDir::Asc))
                }
                TokenKind::Desc => {
                    let span = self.bump().span;
                    Some(Spanned::new(span, SortDir::Desc))
                }
                _ => None,
            };

            match self.parse_expr(true) {
                Ok(expr) => {
                    args.push(Arg { direction, expr });
                }
                Err(()) => {
                    self.sync_to(&[TokenKind::Comma, TokenKind::RParen]);
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
            |k| matches!(k, TokenKind::RParen),
            |span| QueryParseError::UnclosedMethodArgs { span },
        )?;
        Ok(args)
    }
}

fn binary_op(kind: &TokenKind) -> Option<(BinaryOp, u8)> {
    match kind {
        TokenKind::Star => Some((BinaryOp::Mul, 6)),
        TokenKind::Slash => Some((BinaryOp::Div, 6)),
        TokenKind::Percent => Some((BinaryOp::Rem, 6)),
        TokenKind::Mod => Some((BinaryOp::Mod, 6)),
        TokenKind::Plus => Some((BinaryOp::Add, 5)),
        TokenKind::Minus => Some((BinaryOp::Sub, 5)),
        TokenKind::Less => Some((BinaryOp::Lt, 4)),
        TokenKind::Greater => Some((BinaryOp::Gt, 4)),
        TokenKind::LessEquals => Some((BinaryOp::Le, 4)),
        TokenKind::GreaterEquals => Some((BinaryOp::Ge, 4)),
        TokenKind::EqualsEquals => Some((BinaryOp::Eq, 3)),
        TokenKind::BangEquals => Some((BinaryOp::Ne, 3)),
        TokenKind::In => Some((BinaryOp::In, 3)),
        TokenKind::AmpAmp => Some((BinaryOp::And, 2)),
        TokenKind::PipePipe => Some((BinaryOp::Or, 1)),
        _ => None,
    }
}
