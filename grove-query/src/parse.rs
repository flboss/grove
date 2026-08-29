use crate::ast::{
    Arg, BinaryOp, ConstantName, Expr, Literal, MutationKind, MutationStmt, ProjectionItem,
    QueryFile, SortDir, Statement, TypeName, UnaryOp,
};
use crate::error::QueryParseError;
use crate::lex::Lexer;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Span, Spanned};

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
        Span {
            start,
            end: self.current.span.end,
        }
    }

    fn sync_to(&mut self, targets: &[TokenKind]) {
        while !targets.contains(&self.current.value)
            && !matches!(self.current.value, TokenKind::Semicolon | TokenKind::Eof)
        {
            self.bump();
        }
    }

    fn parse_file(&mut self) -> Result<QueryFile, ()> {
        let mut statements = Vec::new();

        loop {
            if matches!(self.current.value, TokenKind::Eof) {
                if self.diagnostics.is_empty() {
                    self.emit_error(QueryParseError::ExpectedExpr {
                        span: self.current.span,
                    });
                }
                return Err(());
            }

            if matches!(self.current.value, TokenKind::Semicolon) {
                self.emit_error(QueryParseError::EmptyStatement {
                    span: self.current.span,
                });
                self.bump();
                continue;
            }

            let Ok(expr) = self.parse_expr(true) else {
                while !matches!(self.current.value, TokenKind::Semicolon | TokenKind::Eof) {
                    self.bump();
                }
                continue;
            };

            if matches!(self.current.value, TokenKind::Semicolon) {
                self.bump();

                if is_mutation(&expr) {
                    statements.push(Statement::Mutation(reclassify_mutation(expr)));
                } else {
                    self.emit_warning(QueryParseError::DiscardedQuery { span: expr.span() });
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
                self.emit_error(QueryParseError::MutationAsResult { span: expr.span() });
                return Err(());
            }

            return Ok(QueryFile {
                statements,
                result: expr,
            });
        }
    }

    fn parse_expr(&mut self, allow_projection_braces: bool) -> Result<Expr, ()> {
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
                op: Spanned {
                    span: op_tok.span,
                    value: op,
                },
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
                span,
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self, allow_projection_braces: bool) -> Result<Expr, ()> {
        match self.current.value {
            TokenKind::Minus => {
                let op_tok = self.bump();
                let expr = self.parse_unary(allow_projection_braces)?;
                let span = self.span_from(op_tok.span.start);
                Ok(Expr::Unary {
                    op: Spanned {
                        span: op_tok.span,
                        value: UnaryOp::Neg,
                    },
                    expr: Box::new(expr),
                    span,
                })
            }
            TokenKind::Bang => {
                let op_tok = self.bump();
                let expr = self.parse_unary(allow_projection_braces)?;
                let span = self.span_from(op_tok.span.start);
                Ok(Expr::Unary {
                    op: Spanned {
                        span: op_tok.span,
                        value: UnaryOp::Not,
                    },
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
                TokenKind::Dot | TokenKind::QuestionDot => {
                    let optional = matches!(self.current.value, TokenKind::QuestionDot);
                    self.bump();
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
                            optional,
                            span,
                        };
                    } else {
                        let span = Span {
                            start: expr.span().start,
                            end: name.span.end,
                        };
                        expr = Expr::Field {
                            base: Box::new(expr),
                            name,
                            optional,
                            span,
                        };
                    }
                }
                TokenKind::LBracket => {
                    let lbracket = self.bump();
                    let cond = self.parse_expr(true)?;
                    self.expect(
                        |k| matches!(k, TokenKind::RBracket),
                        |span| QueryParseError::ExpectedFilterRBracket { span },
                    )?;
                    let filter_name = Spanned {
                        span: lbracket.span,
                        value: "filter".to_string(),
                    };
                    let span = self.span_from(expr.span().start);
                    expr = Expr::Method {
                        base: Box::new(expr),
                        name: filter_name,
                        args: vec![Arg {
                            direction: None,
                            expr: cond,
                        }],
                        optional: false,
                        span,
                    };
                }
                TokenKind::LBrace if allow_projection_braces => {
                    self.bump();
                    let items = self.parse_projection_body()?;
                    self.expect(
                        |k| matches!(k, TokenKind::RBrace),
                        |span| QueryParseError::ExpectedProjectionRBrace { span },
                    )?;
                    let span = self.span_from(expr.span().start);
                    expr = Expr::Projection {
                        base: Box::new(expr),
                        items,
                        span,
                    };
                }
                TokenKind::As => {
                    self.bump();
                    let ty = match self.parse_type_name() {
                        Ok(ty) => ty,
                        Err(found) => {
                            self.emit_error(QueryParseError::ExpectedCastTypeName {
                                span: found.span,
                            });
                            return Err(());
                        }
                    };
                    let span = Span {
                        start: expr.span().start,
                        end: ty.span.end,
                    };
                    expr = Expr::Cast {
                        expr: Box::new(expr),
                        ty,
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
                    Some(Spanned {
                        span,
                        value: SortDir::Asc,
                    })
                }
                TokenKind::Desc => {
                    let span = self.bump().span;
                    Some(Spanned {
                        span,
                        value: SortDir::Desc,
                    })
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

    fn parse_projection_body(&mut self) -> Result<Vec<ProjectionItem>, ()> {
        let mut items = Vec::new();
        while !matches!(self.current.value, TokenKind::RBrace | TokenKind::Eof) {
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
        Ok(items)
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
                span: self.current.span,
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

        while matches!(self.current.value, TokenKind::Dot | TokenKind::QuestionDot) {
            let optional = matches!(self.current.value, TokenKind::QuestionDot);
            self.bump();
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
                optional,
                span,
            };
        }

        Ok(expr)
    }

    fn parse_primary(&mut self) -> Result<Expr, ()> {
        let span = self.current.span;

        match &self.current.value {
            &TokenKind::IntLit(i) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Int(i),
                }))
            }
            &TokenKind::FloatLit(f) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Float(f),
                }))
            }
            &TokenKind::DecLit(d) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Dec(d),
                }))
            }
            TokenKind::StringLit(_) => {
                let TokenKind::StringLit(s) = self.bump().value else {
                    unreachable!()
                };
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::String(s),
                }))
            }
            TokenKind::True => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Bool(true),
                }))
            }
            TokenKind::False => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Bool(false),
                }))
            }
            TokenKind::None => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::None,
                }))
            }
            &TokenKind::InstantLit(dt) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Instant(dt),
                }))
            }
            TokenKind::Now => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Now,
                }))
            }
            &TokenKind::Today(t) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Today(t),
                }))
            }
            &TokenKind::DurationLit(d) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Duration(d),
                }))
            }
            TokenKind::Ident(_) => {
                let name = self.expect_ident()?;
                Ok(Expr::Ident(name))
            }
            TokenKind::Prev => {
                self.bump();
                Ok(Expr::Ident(Spanned {
                    span,
                    value: "prev".to_string(),
                }))
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
            span: Span {
                start: kw.span.start,
                end: rparen.span.end,
            },
        })
    }

    fn parse_type_constant(&mut self) -> Result<Expr, ()> {
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
            name: Spanned {
                span: self.current.span,
                value: constant,
            },
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
                span: Span { start, end },
            });
            return Err(());
        };

        Ok(Expr::If {
            arms,
            default,
            span: Span { start, end },
        })
    }

    fn parse_struct_literal(&mut self) -> Result<Expr, ()> {
        let lbrace = self.bump();
        let mut fields = Vec::new();

        loop {
            let Token { span, value } = &self.current;
            let name = match value {
                TokenKind::Ident(n) => Spanned {
                    span: *span,
                    value: n.clone(),
                },
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
            span: Span {
                start: lbrace.span.start,
                end: rbrace.span.end,
            },
        })
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

    fn parse_type_name(&mut self) -> Result<Spanned<TypeName>, Token> {
        let tok = self.bump();
        let value = match tok.value {
            TokenKind::Int => TypeName::Int,
            TokenKind::Float => TypeName::Float,
            TokenKind::Dec => TypeName::Dec,
            _ => {
                return Err(tok);
            }
        };
        Ok(Spanned {
            span: tok.span,
            value,
        })
    }

    fn emit_error(&mut self, err: QueryParseError) {
        self.diagnostics.push(err.into());
    }

    fn emit_warning(&mut self, err: QueryParseError) {
        self.diagnostics.push(err.into());
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
        base, name, args, ..
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

    let arg = if args.is_empty() {
        None
    } else {
        Some(Box::new(args.into_iter().next().unwrap().expr))
    };

    MutationStmt {
        kind: Spanned {
            span: name.span,
            value: kind,
        },
        base: *base,
        arg,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_query;
    use grove_types::Diagnostic;
    use rust_decimal::Decimal;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    fn parse_ok(src: &str) -> Expr {
        let (file, diags) = crate::parse_query(src);
        assert!(diags.is_empty(), "unexpected diags for `{src}`: {diags:?}");
        file.expect("file should parse").result
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

    #[test]
    fn trailing_input() {
        let (file, diags) = parse_query("42 \"leftover\"");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0002"]);
    }

    #[test]
    fn decimal_literal_expr() {
        match parse_ok("1e10") {
            Expr::Literal(lit) => {
                let Literal::Dec(d) = lit.value else {
                    panic!("expected Dec literal");
                };
                assert_eq!(d, "1e10".parse::<Decimal>().unwrap());
            }
            other => panic!("expected literal, got {other:?}"),
        }
    }

    #[test]
    fn instant_literal_expr() {
        let (file, _) = parse_query("@now");
        match file.unwrap().result {
            Expr::Literal(lit) => assert_eq!(lit.value, Literal::Now),
            other => panic!("expected literal, got {other:?}"),
        }

        let (file, _) = parse_query("@2024-01-15");
        match file.unwrap().result {
            Expr::Literal(lit) => {
                let Literal::Instant(_) = lit.value else {
                    panic!("expected Instant literal");
                };
            }
            other => panic!("expected literal, got {other:?}"),
        }
    }

    #[test]
    fn ident_expr() {
        match parse_ok("users") {
            Expr::Ident(name) => assert_eq!(name.value, "users"),
            other => panic!("expected ident, got {other:?}"),
        }
        match parse_ok("prev") {
            Expr::Ident(name) => assert_eq!(name.value, "prev"),
            other => panic!("expected ident, got {other:?}"),
        }
    }

    #[test]
    fn grouping_parens_dropped() {
        assert!(matches!(parse_ok("(42)"),
            Expr::Literal(lit) if lit.value== Literal::Int(42),
        ));
        assert!(matches!(parse_ok("((true))"),
            Expr::Literal(lit) if lit.value== Literal::Bool(true),
        ));
    }

    #[test]
    fn tuple_expr() {
        match parse_ok("(1, 2)") {
            Expr::Tuple { elements, .. } => {
                assert_eq!(elements.len(), 2);
                let Expr::Literal(lit) = &elements[0] else {
                    panic!("expected literal");
                };
                assert_eq!(lit.value, Literal::Int(1));
            }
            other => panic!("expected tuple, got {other:?}"),
        }
        match parse_ok("(1, 2,)") {
            Expr::Tuple { elements, .. } => assert_eq!(elements.len(), 2),
            other => panic!("expected tuple, got {other:?}"),
        }
    }

    #[test]
    fn single_element_tuple() {
        match parse_ok("(1,)") {
            Expr::Tuple { elements, .. } => {
                assert_eq!(elements.len(), 1);
                let Expr::Literal(lit) = &elements[0] else {
                    panic!("expected literal");
                };
                assert_eq!(lit.value, Literal::Int(1));
            }
            other => panic!("expected tuple, got {other:?}"),
        }
    }

    #[test]
    fn unclosed_tuple() {
        let (file, diags) = parse_query("(1, 2");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0003"]);
    }

    #[test]
    fn array_expr() {
        match parse_ok("[]") {
            Expr::Array { elements, .. } => assert!(elements.is_empty()),
            other => panic!("expected array, got {other:?}"),
        }
        match parse_ok("[{ name = \"a\" }]") {
            Expr::Array { elements, .. } => {
                assert_eq!(elements.len(), 1);
                let Expr::Struct { fields, .. } = &elements[0] else {
                    panic!("expected struct literal");
                };
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0.value, "name");
            }
            other => panic!("expected array, got {other:?}"),
        }
    }

    #[test]
    fn unclosed_array() {
        let (file, diags) = parse_query("[1, 2");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0005"]);
    }

    #[test]
    fn some_expr() {
        match parse_ok("some(42)") {
            Expr::Some { value, .. } => {
                let Expr::Literal(lit) = &*value else {
                    panic!("expected literal");
                };
                assert_eq!(lit.value, Literal::Int(42));
            }
            other => panic!("expected some, got {other:?}"),
        }
    }

    #[test]
    fn malformed_some_expr() {
        let (file, diags) = parse_query("some 42");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0006"]);

        let (file, diags) = parse_query("some(42");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0007"]);
    }

    #[test]
    fn type_constants() {
        match parse_ok("Int::MIN") {
            Expr::TypeConstant { ty, name } => {
                assert_eq!(ty.value, TypeName::Int);
                assert_eq!(name.value, ConstantName::Min);
            }
            other => panic!("expected type constant, got {other:?}"),
        }
        match parse_ok("Float::MAX") {
            Expr::TypeConstant { ty, name } => {
                assert_eq!(ty.value, TypeName::Float);
                assert_eq!(name.value, ConstantName::Max);
            }
            other => panic!("expected type constant, got {other:?}"),
        }
    }

    #[test]
    fn malformed_type_constants() {
        let (file, diags) = parse_query("Int FOO");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0008"]);

        let (file, diags) = parse_query("Int::FOO");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0009"]);
    }

    #[test]
    fn if_else_expr() {
        let src = "if active { 1 } else if retired { 2 } else { 3 }";
        match parse_ok(src) {
            Expr::If {
                arms,
                default,
                span,
            } => {
                assert_eq!(arms.len(), 2);
                let Expr::Literal(lit) = &*default else {
                    panic!("expected literal in else");
                };
                assert_eq!(lit.value, Literal::Int(3));
                assert_eq!(span.start, 0);
                assert_eq!(span.end, src.len());
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    #[test]
    fn missing_else() {
        let src = "if active { 1 }";
        let (file, diags) = parse_query(src);
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0012"]);
        let diag = &diags[0];
        let label = &diag.labels[0];
        assert_eq!(label.span.start, 0);
        assert_eq!(label.span.end, src.len());
    }

    #[test]
    fn malformed_if_blocks() {
        let (file, diags) = parse_query("if active 1 else { 2 }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0010"]);

        let (file, diags) = parse_query("if active { 1");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0011"]);

        let (file, diags) = parse_query("if active { 1 } else 2");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0013"]);
    }

    #[test]
    fn parenthesized_if_condition() {
        match parse_ok("if (true) { 1 } else { 2 }") {
            Expr::If { arms, .. } => {
                let Expr::Literal(lit) = &arms[0].0 else {
                    panic!("expected literal condition");
                };
                assert_eq!(lit.value, Literal::Bool(true));
            }
            other => panic!("expected if, got {other:?}"),
        }
        parse_ok("if ((true)) { 1 } else { 2 }");
    }

    #[test]
    fn unclosed_explicit_if_condition_parens() {
        let (file, diags) = parse_query("if (true { 1 } else { 2 }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0022", "QP0003"]);
    }

    #[test]
    fn struct_literal_expr() {
        match parse_ok("{ name = 1 }") {
            Expr::Struct { fields, span } => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0.value, "name");
                assert_eq!(span.start, 0);
                assert_eq!(span.end, 12);
            }
            other => panic!("expected struct literal, got {other:?}"),
        }

        match parse_ok("if { name = 1 } { 2 } else { 3 }") {
            Expr::If { arms, .. } => {
                assert!(matches!(&arms[0].0, Expr::Struct { .. }));
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    #[test]
    fn malformed_struct_literal() {
        let (file, diags) = parse_query("{ }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0014"]);

        let (file, diags) = parse_query("{ name }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0015"]);

        let (file, diags) = parse_query("{ name = }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }

    #[test]
    fn method_call_no_args() {
        let expr = parse_ok("users.len()");
        assert!(matches!(expr, Expr::Method {
            name: Spanned { value: ref n, .. },
            args,
            ..
        } if n == "len" && args.is_empty()));
    }

    #[test]
    fn method_call_with_args() {
        let expr = parse_ok("user.sort_asc(name)");
        assert!(matches!(expr, Expr::Method {
            name: Spanned { value: ref n, .. },
            args,
            ..
        } if n == "sort_asc" && args.len() == 1));
    }

    #[test]
    fn method_call_sort_directions() {
        let expr = parse_ok("user.sort(asc name, desc age)");
        let Expr::Method { args, .. } = expr else {
            panic!()
        };
        assert_eq!(args.len(), 2);
        assert!(args[0].direction.is_some());
        assert!(args[1].direction.is_some());
    }

    #[test]
    fn bracket_filter_method_desugar() {
        let expr = parse_ok("users[active]");
        assert!(matches!(expr, Expr::Method {
            name: Spanned { value: ref n, .. },
            args,
            ..
        } if n == "filter" && args.len() == 1));
    }

    #[test]
    fn unclosed_filter() {
        let (file, diags) = parse_query("users[active");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0019"]);
    }

    #[test]
    fn as_cast() {
        let expr = parse_ok("1 as Int");
        assert!(matches!(
            expr,
            Expr::Cast {
                ty: Spanned {
                    value: TypeName::Int,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn as_cast_unknown_type() {
        let (_, diags) = parse_query("1 as Something");
        assert_eq!(codes(&diags), vec!["QP0021"]);
    }

    #[test]
    fn empty_projection() {
        let expr = parse_ok("users { }");
        assert!(matches!(expr, Expr::Projection { items, .. } if items.is_empty()));
    }

    #[test]
    fn projection_plain_fields() {
        let expr = parse_ok("users { name, email }");
        assert!(matches!(expr, Expr::Projection { items, .. }
            if items.len() == 2 && items.iter().all(|i| i.alias.is_none())
        ));
    }

    #[test]
    fn projection_aliased_computed() {
        let expr = parse_ok("users { total = some(1) }");
        let Expr::Projection { items, .. } = expr else {
            panic!()
        };
        assert_eq!(items.len(), 1);
        assert!(items[0].alias.is_some());
    }

    #[test]
    fn projection_nested_aliased() {
        let expr = parse_ok("users { orders = orders { total } }");
        let Expr::Projection { items, .. } = expr else {
            panic!()
        };
        assert_eq!(items.len(), 1);
        assert!(items[0].alias.is_some());
    }

    #[test]
    fn projection_nested_missing_alias() {
        let (file, diags) = parse_query("users { orders { total } }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0022", "QP0002"]);
    }

    #[test]
    fn unclosed_projection() {
        let (file, diags) = parse_query("users { name");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0020"]);
    }

    #[test]
    fn missing_ident_after_dot() {
        let (_, diags) = parse_query("user.");
        assert_eq!(codes(&diags), vec!["QP0017"]);
    }

    #[test]
    fn postfixes() {
        let expr = parse_ok("users[x].sort(asc name).take(5) { total }");
        let Expr::Projection { base, items, .. } = expr else {
            panic!("expected projection");
        };
        assert!(items.len() == 1);
        let Expr::Method { base, name, .. } = *base else {
            panic!("expected method call");
        };
        assert_eq!(name.value, "take");
        let Expr::Method {
            base, name, args, ..
        } = *base
        else {
            panic!("expected method call");
        };
        assert_eq!(name.value, "sort");
        assert_eq!(
            **args.first().unwrap().direction.as_ref().unwrap(),
            SortDir::Asc
        );
        let Expr::Method {
            base, name, args, ..
        } = *base
        else {
            panic!("expected filter");
        };
        assert_eq!(name.value, "filter");
        assert!(matches!(args.first().unwrap().expr, Expr::Ident(_)));
        assert!(matches!(*base, Expr::Ident(_)));
    }

    #[test]
    fn unary_neg() {
        match parse_ok("-1") {
            Expr::Unary { op, expr, .. } => {
                assert_eq!(op.value, UnaryOp::Neg);
                assert!(matches!(*expr, Expr::Literal(lit)
                    if lit.value == Literal::Int(1)
                ));
            }
            other => panic!("expected unary, got {other:?}"),
        }
    }

    #[test]
    fn unary_not() {
        match parse_ok("!true") {
            Expr::Unary { op, expr, .. } => {
                assert_eq!(op.value, UnaryOp::Not);
                assert!(matches!(*expr, Expr::Literal(lit)
                    if lit.value == Literal::Bool(true)
                ));
            }
            other => panic!("expected unary, got {other:?}"),
        }
    }

    #[test]
    fn double_unary_neg() {
        match parse_ok("--1") {
            Expr::Unary { op, expr, .. } => {
                assert_eq!(op.value, UnaryOp::Neg);
                match *expr {
                    Expr::Unary { op, expr, .. } => {
                        assert_eq!(op.value, UnaryOp::Neg);
                        assert!(matches!(*expr, Expr::Literal(lit)
                            if lit.value == Literal::Int(1)
                        ));
                    }
                    other => panic!("expected inner unary, got {other:?}"),
                }
            }
            other => panic!("expected unary, got {other:?}"),
        }
    }

    #[test]
    fn binary_add() {
        match parse_ok("1 + 2") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::Add);
                assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
                assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(2)));
            }
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_sub() {
        match parse_ok("1 - 2") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Sub),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_mul() {
        match parse_ok("1 * 2") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Mul),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_div() {
        match parse_ok("1 / 2") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Div),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_rem() {
        match parse_ok("1 % 2") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Rem),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_mod() {
        match parse_ok("1 mod 2") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Mod),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_eq() {
        match parse_ok("a == b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Eq),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_ne() {
        match parse_ok("a != b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Ne),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_lt() {
        match parse_ok("a < b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Lt),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_gt() {
        match parse_ok("a > b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Gt),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_le() {
        match parse_ok("a <= b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Le),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_ge() {
        match parse_ok("a >= b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Ge),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_and() {
        match parse_ok("a && b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::And),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_or() {
        match parse_ok("a || b") {
            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Or),
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn binary_in() {
        match parse_ok("a in (1, 2)") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::In);
                assert!(matches!(*lhs, Expr::Ident(_)));
                assert!(matches!(*rhs, Expr::Tuple { .. }));
            }
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn mul_add_precedence() {
        match parse_ok("1 + 2 * 3") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::Add);
                assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
                match *rhs {
                    Expr::Binary { op, lhs, rhs, .. } => {
                        assert_eq!(op.value, BinaryOp::Mul);
                        assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(2)));
                        assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(3)));
                    }
                    other => panic!("expected mul, got {other:?}"),
                }
            }
            other => panic!("expected add, got {other:?}"),
        }
    }

    #[test]
    fn left_associativity() {
        match parse_ok("1 - 2 + 3") {
            Expr::Binary { op, lhs, .. } => {
                assert_eq!(op.value, BinaryOp::Add);
                match *lhs {
                    Expr::Binary { op, lhs, rhs, .. } => {
                        assert_eq!(op.value, BinaryOp::Sub);
                        assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
                        assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(2)));
                    }
                    other => panic!("expected sub, got {other:?}"),
                }
            }
            other => panic!("expected add, got {other:?}"),
        }
    }

    #[test]
    fn parens_override_precedence() {
        match parse_ok("(1 + 2) * 3") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::Mul);
                match *lhs {
                    Expr::Binary { op, .. } => {
                        assert_eq!(op.value, BinaryOp::Add);
                    }
                    other => panic!("expected add, got {other:?}"),
                }
                assert!(matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(3)));
            }
            other => panic!("expected mul, got {other:?}"),
        }
    }

    #[test]
    fn cast_binary_precedence() {
        match parse_ok("1 + 2 as Int") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::Add);
                assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
                assert!(matches!(*rhs, Expr::Cast { .. }));
            }
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn unary_in_binary_rhs() {
        match parse_ok("1 + -2") {
            Expr::Binary { op, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::Add);
                assert!(matches!(*rhs, Expr::Unary { op, .. } if op.value == UnaryOp::Neg));
            }
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn mixed_precedence() {
        match parse_ok("1 + 2 * 3 <= 4.0 as Int - 5 / 6 % 7") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::Le);
                match *lhs {
                    Expr::Binary { op, lhs, rhs, .. } => {
                        assert_eq!(op.value, BinaryOp::Add);
                        assert!(matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(1)));
                        match *rhs {
                            Expr::Binary { op, .. } => assert_eq!(op.value, BinaryOp::Mul),
                            other => panic!("expected mul, got {other:?}"),
                        }
                    }
                    other => panic!("expected add, got {other:?}"),
                }
                match *rhs {
                    Expr::Binary { op, lhs, rhs, .. } => {
                        assert_eq!(op.value, BinaryOp::Sub);
                        match *lhs {
                            Expr::Cast { ty, .. } => assert_eq!(ty.value, TypeName::Int),
                            other => panic!("expected cast, got {other:?}"),
                        }
                        match *rhs {
                            Expr::Binary { op, lhs, rhs, .. } => {
                                assert_eq!(op.value, BinaryOp::Rem);
                                match *lhs {
                                    Expr::Binary { op, lhs, rhs, .. } => {
                                        assert_eq!(op.value, BinaryOp::Div);
                                        assert!(
                                            matches!(*lhs, Expr::Literal(lit) if lit.value == Literal::Int(5))
                                        );
                                        assert!(
                                            matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(6))
                                        );
                                    }
                                    other => panic!("expected cast, got {other:?}"),
                                }
                                assert!(
                                    matches!(*rhs, Expr::Literal(lit) if lit.value == Literal::Int(7))
                                );
                            }
                            other => panic!("expected mul, got {other:?}"),
                        }
                    }
                    other => panic!("expected sub, got {other:?}"),
                }
            }
            other => panic!("expected le, got {other:?}"),
        }
    }

    #[test]
    fn binary_missing_rhs() {
        let (file, diags) = parse_query("1 +");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }

    #[test]
    fn binary_missing_lhs() {
        let (file, diags) = parse_query("+ 1");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }

    #[test]
    fn unary_neg_binary_sub() {
        match parse_ok("a - -b") {
            Expr::Binary { op, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::Sub);
                assert!(matches!(*rhs, Expr::Unary { op, .. } if op.value == UnaryOp::Neg));
            }
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn comparison_precedence() {
        let expr = parse_ok("a < b && c >= d");
        assert!(matches!(expr, Expr::Binary { op, .. } if op.value == BinaryOp::And));
        match parse_ok("a < b && c >= d") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::And);
                assert!(matches!(*lhs, Expr::Binary { op, .. } if op.value == BinaryOp::Lt));
                assert!(matches!(*rhs, Expr::Binary { op, .. } if op.value == BinaryOp::Ge));
            }
            other => panic!("expected binary, got {other:?}"),
        }
    }

    #[test]
    fn if_condition_expr() {
        match parse_ok("if x == 0 { 1 } else { 2 }") {
            Expr::If { arms, .. } => {
                assert!(matches!(&arms[0].0, Expr::Binary { op, .. } if op.value == BinaryOp::Eq));
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    #[test]
    fn mutation_insert_statement() {
        let src = "users.insert({name = \"Alice\"}); 0";
        let (file, diags) = parse_query(src);
        let file = file.expect("should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(
            matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Insert)
        );
        assert!(matches!(file.result, Expr::Literal(lit) if lit.value == Literal::Int(0)));
        assert!(
            diags
                .iter()
                .all(|d| d.severity != grove_types::Severity::Warning)
        );
    }

    #[test]
    fn mutation_delete_statement() {
        let src = "users[!active].delete(); 0";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(
            matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Delete)
        );
    }

    #[test]
    fn mutation_update_statement() {
        let src = "users.update({name = \"Bob\"}); 0";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(
            matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Update)
        );
    }

    #[test]
    fn discarded_query_warning() {
        let src = "users { name }; 0";
        let (file, diags) = parse_query(src);
        let file = file.expect("should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(matches!(&file.statements[0], Statement::Expr(_)));
        let warnings: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == grove_types::Severity::Warning)
            .collect();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].code.as_ref(), "QP0024");
    }

    #[test]
    fn empty_statement_error() {
        let (file, diags) = parse_query(";");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0023"));
    }

    #[test]
    fn multiple_statements() {
        let src = "users.insert({name = \"Alice\"}); users.insert({name = \"Bob\"}); 42";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 2);
        assert!(
            file.statements
                .iter()
                .all(|s| matches!(s, Statement::Mutation(_)))
        );
        assert!(matches!(file.result, Expr::Literal(lit) if lit.value == Literal::Int(42)));
    }

    #[test]
    fn statement_and_discarded() {
        let src = "users.insert({name = \"Alice\"}); users { name }; 42";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 2);
        assert!(matches!(&file.statements[0], Statement::Mutation(_)));
        assert!(matches!(&file.statements[1], Statement::Expr(_)));
    }

    #[test]
    fn missing_result() {
        let (file, diags) = parse_query("users.insert({name = \"Alice\"});");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0001"));
    }

    #[test]
    fn mutation_as_result_error() {
        let (file, diags) = parse_query("users.insert({name = \"Alice\"})");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0025"));
    }

    #[test]
    fn multi_error_projection_items() {
        let (file, diags) = parse_query("users { orders { total }, 42 }");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0022", "QP0002"]);
    }

    #[test]
    fn multi_error_method_args() {
        let (_, diags) = parse_query("users.sort(asc , asc )");
        assert_eq!(codes(&diags), vec!["QP0001", "QP0001"]);
    }

    #[test]
    fn multi_error_array_elements() {
        let (file, diags) = parse_query("[,]");
        assert!(file.is_some());
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }

    #[test]
    fn recovery_skips_to_comma_in_args() {
        let (_, diags) = parse_query("users.sort(name, , age)");
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }

    #[test]
    fn recovery_skips_to_comma_in_projection() {
        let (file, diags) = parse_query("users { , name }");
        assert!(file.is_some());
        assert_eq!(codes(&diags), vec!["QP0022"]);
    }

    #[test]
    fn sync_to_stops_at_semicolon() {
        let src = "users.sort(bad; 42";
        let (file, diags) = parse_query(src);
        let file = file.expect("should parse");
        assert!(matches!(file.result, Expr::Literal(lit) if lit.value == Literal::Int(42)));
        let errors: Vec<_> = diags
            .iter()
            .filter(|d| d.severity == grove_types::Severity::Error)
            .collect();
        assert!(!errors.is_empty());
    }
}
