use std::collections::HashMap;

use crate::ast::{
    Arg, BinaryOp, ConstantName, Expr, LetStmt, Literal, MutationKind, MutationStmt,
    ProjectionItem, QueryFile, SortDir, Statement, TypeExpr, TypeName, UnaryOp,
};
use crate::error::QueryParseError;
use crate::lex::Lexer;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Severity, Span, Spanned};

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

    fn parse_file(&mut self) -> Result<QueryFile, ()> {
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

    fn parse_projection_body(&mut self) -> Result<(Vec<ProjectionItem>, Option<Span>), ()> {
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

    fn parse_primary(&mut self) -> Result<Expr, ()> {
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
        // TokenKind::String isn't dispatched here
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
            TokenKind::String => TypeName::String,
            TokenKind::Bytes => TypeName::Bytes,
            TokenKind::Bool => TypeName::Bool,
            TokenKind::Instant => TypeName::Instant,
            TokenKind::Duration => TypeName::Duration,
            _ => {
                return Err(tok);
            }
        };
        Ok(Spanned::new(tok.span, value))
    }

    fn parse_type_expr(&mut self) -> Result<Spanned<TypeExpr>, ()> {
        if let &TokenKind::Question(depth) = &self.current.value {
            let question = self.bump();
            let mut ty = self.parse_type_expr()?;
            let span = Span::new(question.span.start, ty.span.end);
            for _ in 0..depth {
                ty = Spanned::new(
                    span,
                    TypeExpr::Optional {
                        inner: Box::new(ty),
                        span,
                    },
                );
            }
            return Ok(ty);
        }
        self.parse_type_primary()
    }

    fn parse_type_primary(&mut self) -> Result<Spanned<TypeExpr>, ()> {
        match &self.current.value {
            TokenKind::Int => Ok(self.scalar_type(TypeName::Int)),
            TokenKind::Float => Ok(self.scalar_type(TypeName::Float)),
            TokenKind::Dec => Ok(self.scalar_type(TypeName::Dec)),
            TokenKind::String => Ok(self.scalar_type(TypeName::String)),
            TokenKind::Bytes => Ok(self.scalar_type(TypeName::Bytes)),
            TokenKind::Bool => Ok(self.scalar_type(TypeName::Bool)),
            TokenKind::Instant => Ok(self.scalar_type(TypeName::Instant)),
            TokenKind::Duration => Ok(self.scalar_type(TypeName::Duration)),
            TokenKind::List => self.parse_list_type(),
            TokenKind::LParen => self.parse_tuple_type(),
            TokenKind::LBrace => self.parse_record_type(),
            TokenKind::Ident(_) => {
                let tok = self.bump();
                let TokenKind::Ident(name) = tok.value else {
                    unreachable!()
                };
                Ok(Spanned::new(tok.span, TypeExpr::Named(name)))
            }
            _ => {
                let span = self.current.span;
                self.emit_error(QueryParseError::ExpectedType { span });
                Err(())
            }
        }
    }

    fn scalar_type(&mut self, ty: TypeName) -> Spanned<TypeExpr> {
        let tok = self.bump();
        Spanned::new(tok.span, TypeExpr::Scalar(ty))
    }

    fn parse_list_type(&mut self) -> Result<Spanned<TypeExpr>, ()> {
        let list = self.bump();
        if !matches!(self.current.value, TokenKind::Less) {
            let span = self.current.span;
            self.emit_error(QueryParseError::ExpectedListTypeLAngle { span });
            return Err(());
        }
        self.bump();

        let element = self.parse_type_expr()?;
        if !matches!(self.current.value, TokenKind::Greater) {
            let span = self.current.span;
            self.emit_error(QueryParseError::ExpectedListTypeRAngle { span });
            return Err(());
        }
        let rangle = self.bump();
        let span = Span::new(list.span.start, rangle.span.end);
        Ok(Spanned::new(
            span,
            TypeExpr::List {
                element: Box::new(element),
                span,
            },
        ))
    }

    fn parse_tuple_type(&mut self) -> Result<Spanned<TypeExpr>, ()> {
        let lparen = self.bump();
        if matches!(self.current.value, TokenKind::RParen) {
            let span = self.span_from(lparen.span.start);
            self.emit_error(QueryParseError::EmptyTupleType { span });
            return Err(());
        }

        let mut elements = vec![self.parse_type_expr()?];
        let rparen = loop {
            if matches!(self.current.value, TokenKind::RParen) {
                break self.bump();
            }
            if !matches!(self.current.value, TokenKind::Comma) {
                let span = self.current.span;
                self.emit_error(QueryParseError::TupleCommaOrRParenExpected { span });
                return Err(());
            }
            self.bump();
            elements.push(self.parse_type_expr()?);
        };

        let span = lparen.span.merge(rparen.span);
        Ok(Spanned::new(span, TypeExpr::Tuple { elements, span }))
    }

    fn parse_record_type(&mut self) -> Result<Spanned<TypeExpr>, ()> {
        let lbrace = self.bump();
        let mut fields = Vec::new();
        let mut field_spans: HashMap<String, Span> = HashMap::new();
        let rbrace = loop {
            if matches!(self.current.value, TokenKind::RBrace) {
                break self.bump();
            }

            let Ok(name) = self.expect_ident() else {
                let span = self.current.span;
                self.emit_error(QueryParseError::ExpectedTypeFieldName { span });
                return Err(());
            };
            if !matches!(self.current.value, TokenKind::Colon) {
                let span = self.current.span;
                self.emit_error(QueryParseError::ExpectedTypeFieldColon { span });
                return Err(());
            }
            self.bump();

            let ty = self.parse_type_expr()?;
            if let Some(previous) = field_spans.get(&name.value) {
                self.emit_error(QueryParseError::DuplicateTypeField {
                    span: name.span,
                    field: name.value.clone(),
                    previous: *previous,
                });
                return Err(());
            }
            field_spans.insert(name.value.clone(), name.span);
            fields.push((name, ty));

            match self.current.value {
                TokenKind::Comma => {
                    self.bump();
                }
                TokenKind::RBrace => {}
                _ => {
                    let span = self.current.span;
                    self.emit_error(QueryParseError::ExpectedTypeFieldCommaOrRBrace { span });
                    return Err(());
                }
            }
        };

        let span = lbrace.span.merge(rbrace.span);
        if fields.is_empty() {
            self.emit_error(QueryParseError::EmptyRecordType { span });
            return Err(());
        }
        Ok(Spanned::new(span, TypeExpr::Record { fields, span }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_query;
    use grove_types::Diagnostic;
    use rust_decimal::dec;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    fn parse_ok(src: &str) -> Expr {
        let (file, diags) = crate::parse_query(src);
        assert!(diags.is_empty(), "unexpected diags for `{src}`: {diags:?}");
        file.expect("file should parse")
            .into_statements_and_output()
            .1
            .expect("file should have output")
    }

    #[test]
    fn empty_is_valid_without_output() {
        let (file, diags) = parse_query("");
        let file = file.expect("empty file should parse");
        assert!(file.statements.is_empty());
        assert!(file.output().is_none());
        assert!(diags.is_empty());
    }

    #[test]
    fn comment_is_valid_without_output() {
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
                matches!(
                    crate::parse_query_repl(src),
                    crate::ParseOutcome::Complete { .. }
                ),
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
                matches!(
                    crate::parse_query_repl(src),
                    crate::ParseOutcome::Incomplete { .. }
                ),
                "expected incomplete parse for `{src}`"
            );
        }
    }

    #[test]
    fn repl_parse_invalid() {
        for src in [";", "let x 1;", "42 \"leftover\""] {
            assert!(
                matches!(
                    crate::parse_query_repl(src),
                    crate::ParseOutcome::Invalid { .. }
                ),
                "expected invalid parse for `{src}`"
            );
        }
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
                assert_eq!(d, dec!(1e10));
            }
            other => panic!("expected literal, got {other:?}"),
        }
    }

    #[test]
    fn instant_literal_expr() {
        match parse_ok("@now") {
            Expr::Literal(lit) => assert_eq!(lit.value, Literal::Now),
            other => panic!("expected literal, got {other:?}"),
        }

        match parse_ok("@2024-01-15") {
            Expr::Literal(lit) => {
                let Literal::Instant(_) = lit.value else {
                    panic!("expected Instant literal");
                };
            }
            other => panic!("expected literal, got {other:?}"),
        }
    }

    #[test]
    fn bytes_literal_expr() {
        let src = r#"hex"DEAD_be ef""#;
        match parse_ok(src) {
            Expr::Literal(lit) => {
                assert_eq!(lit.value, Literal::Bytes(vec![0xde, 0xad, 0xbe, 0xef]));
                assert_eq!(lit.span.start, 0);
                assert_eq!(lit.span.end, src.len());
            }
            other => panic!("expected literal, got {other:?}"),
        }

        match parse_ok(r#"b64"Zm9v""#) {
            Expr::Literal(lit) => assert_eq!(lit.value, Literal::Bytes(b"foo".to_vec())),
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
    fn tuple_access_single() {
        let expr = parse_ok("pos.1");
        assert!(matches!(
            expr,
            Expr::TupleAccess {
                index: Spanned { value: 1, .. },
                opt_depth: 0,
                ..
            }
        ));
    }

    #[test]
    fn tuple_access_chained() {
        let expr = parse_ok("trio.2.1");
        assert!(matches!(expr, Expr::TupleAccess {
            base,
            index: Spanned { value: 1, .. },
            ..
        } if matches!(base.as_ref(), Expr::TupleAccess {
            index: Spanned { value: 2, .. },
            ..
        })));
    }

    #[test]
    fn tuple_access_optional() {
        let expr = parse_ok("t?.2");
        assert!(matches!(
            expr,
            Expr::TupleAccess {
                index: Spanned { value: 2, .. },
                opt_depth: 1,
                ..
            }
        ));
    }

    #[test]
    fn optional_access_depths() {
        assert!(matches!(
            parse_ok("foo??.bar"),
            Expr::Field { opt_depth: 2, .. }
        ));
        assert!(matches!(
            parse_ok("foo???.0"),
            Expr::TupleAccess { opt_depth: 3, .. }
        ));
        assert!(matches!(
            parse_ok("foo????.map($ + 1)"),
            Expr::Method { opt_depth: 4, .. }
        ));

        let Expr::Projection { items, .. } = parse_ok("users { manager??.name }") else {
            panic!("expected projection");
        };
        assert!(matches!(&items[0].value, Expr::Field { opt_depth: 2, .. }));
    }

    #[test]
    fn dollar_as_ident() {
        let expr = parse_ok("[1, 2].filter($ > 0)");
        let Expr::Method { args, .. } = expr else {
            panic!("expected method");
        };
        assert_eq!(args.len(), 1);
        assert!(matches!(&args[0].expr, Expr::Binary { lhs, .. }
            if matches!(lhs.as_ref(), Expr::Ident(name) if name.value == "$")));
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
                    value: TypeExpr::Scalar(TypeName::Int),
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn as_cast_bytes() {
        let expr = parse_ok(r#""abc" as Bytes"#);
        assert!(matches!(
            expr,
            Expr::Cast {
                ty: Spanned {
                    value: TypeExpr::Scalar(TypeName::Bytes),
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn as_fallible_cast() {
        let expr = parse_ok("1 as? Int");
        assert!(matches!(
            expr,
            Expr::Cast {
                fallible: true,
                ty: Spanned {
                    value: TypeExpr::Scalar(TypeName::Int),
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn as_fallible_cast_greedy_question() {
        let expr = parse_ok("1 as??Int");
        assert!(matches!(
            expr,
            Expr::Cast {
                fallible: true,
                ty: Spanned {
                    value: TypeExpr::Optional { .. },
                    ..
                },
                ..
            }
        ));
        let expr = parse_ok("1 as ?Int");
        assert!(matches!(
            expr,
            Expr::Cast {
                fallible: false,
                ty: Spanned {
                    value: TypeExpr::Optional { .. },
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn enum_variant_expr() {
        let expr = parse_ok("Priority::High");
        match expr {
            Expr::EnumVariant {
                enum_name,
                variant,
                span,
            } => {
                assert_eq!(enum_name.value, "Priority");
                assert_eq!(variant.value, "High");
                assert_eq!(span.start, 0);
                assert_eq!(span.end, "Priority::High".len());
            }
            other => panic!("expected enum variant, got {other:?}"),
        }
    }

    #[test]
    fn bare_ident_not_enum_variant() {
        let expr = parse_ok("Priority");
        assert!(matches!(expr, Expr::Ident(name) if name.value == "Priority"));
    }

    #[test]
    fn enum_variant_missing_name() {
        let (file, diags) = parse_query("Priority::");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0030"]);
    }

    #[test]
    fn enum_variant_cast_to_string() {
        let expr = parse_ok("Priority::High as String");
        assert!(matches!(
            expr,
            Expr::Cast {
                expr,
                ty: Spanned {
                    value: TypeExpr::Scalar(TypeName::String),
                    ..
                },
                ..
            } if matches!(expr.as_ref(), Expr::EnumVariant { .. })
        ));
    }

    fn cast_type(src: &str) -> TypeExpr {
        match parse_ok(src) {
            Expr::Cast { ty, .. } => ty.value,
            other => panic!("expected cast, got {other:?}"),
        }
    }

    #[test]
    fn as_cast_named_type() {
        let expr = parse_ok("1 as Something");
        assert!(matches!(
            expr,
            Expr::Cast {
                ty: Spanned {
                    value: TypeExpr::Named(name),
                    ..
                },
                ..
            } if name == "Something"
        ));
    }

    #[test]
    fn as_cast_scalars_and_named() {
        for (source, name) in [
            ("1 as Int", TypeName::Int),
            ("1 as Float", TypeName::Float),
            ("1 as Dec", TypeName::Dec),
            ("1 as String", TypeName::String),
            ("1 as Bytes", TypeName::Bytes),
            ("1 as Bool", TypeName::Bool),
            ("1 as Instant", TypeName::Instant),
            ("1 as Duration", TypeName::Duration),
        ] {
            assert_eq!(
                cast_type(source),
                TypeExpr::Scalar(name),
                "source: {source}"
            );
        }
        assert_eq!(cast_type("1 as User"), TypeExpr::Named("User".to_string()));
    }

    #[test]
    fn type_annotation_optional_list_tuple_record() {
        assert!(matches!(
            cast_type("none as ?String"),
            TypeExpr::Optional { inner, .. }
                if inner.value == TypeExpr::Scalar(TypeName::String)
        ));
        assert!(matches!(
            cast_type("some(none) as ??String"),
            TypeExpr::Optional { inner, .. }
                if matches!(
                    &inner.value,
                    TypeExpr::Optional { inner, .. }
                        if inner.value == TypeExpr::Scalar(TypeName::String)
                )
        ));
        assert!(matches!(
            cast_type("[] as List<Int>"),
            TypeExpr::List { element, .. }
                if element.value == TypeExpr::Scalar(TypeName::Int)
        ));
        assert!(matches!(
            cast_type("(1,) as (Int)"),
            TypeExpr::Tuple { ref elements, .. } if elements.len() == 1
        ));
        assert!(matches!(
            cast_type("(1, \"\") as (Int, String)"),
            TypeExpr::Tuple { ref elements, .. } if elements.len() == 2
        ));

        let record = cast_type("1 as {a: Int, b: List<String>,}");
        let TypeExpr::Record { fields, .. } = record else {
            panic!("expected record type");
        };
        assert_eq!(fields.len(), 2);
        assert_eq!(fields[0].0.value, "a");
        assert_eq!(fields[0].1.value, TypeExpr::Scalar(TypeName::Int));
        assert_eq!(fields[1].0.value, "b");
        assert!(matches!(
            fields[1].1.value,
            TypeExpr::List { ref element, .. }
                if element.value == TypeExpr::Scalar(TypeName::String)
        ));
    }

    #[test]
    fn type_annotation_complex() {
        let ty = cast_type("[] as List<(?Int, List<Duration>, ??String, {a: Instant, d: Dec})>");
        let TypeExpr::List { element, .. } = ty else {
            panic!("expected list type");
        };
        let TypeExpr::Tuple { elements, .. } = element.value else {
            panic!("expected tuple type");
        };
        assert_eq!(elements.len(), 4);
        assert!(matches!(
            elements[0].value,
            TypeExpr::Optional { ref inner, .. }
                if inner.value == TypeExpr::Scalar(TypeName::Int)
        ));
        assert!(matches!(
            elements[1].value,
            TypeExpr::List { ref element, .. }
                if element.value == TypeExpr::Scalar(TypeName::Duration)
        ));
        assert!(matches!(
            elements[2].value,
            TypeExpr::Optional { ref inner, .. }
                if matches!(&inner.value, TypeExpr::Optional { inner, .. }
                    if inner.value == TypeExpr::Scalar(TypeName::String))
        ));
        assert!(matches!(
            elements[3].value,
            TypeExpr::Record { ref fields, .. } if fields.len() == 2
        ));
    }

    #[test]
    fn type_annotation_errors() {
        for (source, code) in [
            ("1 as", "QP0021"),
            ("1 as ()", "QP0031"),
            ("1 as {}", "QP0032"),
            ("1 as {,}", "QP0033"),
            ("1 as {a Int}", "QP0034"),
            ("1 as {a: Int b: String}", "QP0035"),
            ("1 as List Int", "QP0036"),
            ("1 as List<Int", "QP0037"),
            ("1 as {a: Int, a: String}", "QP0038"),
            ("1 as (Int String)", "QP0003"),
        ] {
            let (file, diags) = parse_query(source);
            assert!(file.is_none(), "source: {source}");
            assert_eq!(codes(&diags), vec![code], "source: {source}");
        }
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
    fn projection_spread_only() {
        let expr = parse_ok("users { ... }");
        assert!(matches!(
            expr,
            Expr::Projection {
                items,
                spread: Some(_),
                ..
            } if items.is_empty()
        ));
    }

    #[test]
    fn projection_spread_last() {
        let expr = parse_ok("users { name, ..., }");
        let Expr::Projection {
            items,
            spread: Some(_),
            ..
        } = expr
        else {
            panic!("expected projection with spread");
        };
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].alias, None);
    }

    #[test]
    fn projection_spread_not_last() {
        for source in ["users { ..., name }", "users { name, ..., age, }"] {
            let (file, diags) = parse_query(source);
            assert!(file.is_none(), "source: {source}");
            assert_eq!(codes(&diags), vec!["QP0040"], "source: {source}");
        }
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
    fn row_ref() {
        match parse_ok("&users") {
            Expr::RowRef { expr, .. } => {
                assert!(matches!(*expr, Expr::Ident(name) if name.value == "users"));
            }
            other => panic!("expected row ref, got {other:?}"),
        }
    }

    #[test]
    fn row_ref_precedence_over_postfix_chain() {
        match parse_ok("&users.first()") {
            Expr::RowRef { expr, .. } => match *expr {
                Expr::Method { base, name, .. } => {
                    assert_eq!(name.value, "first");
                    assert!(matches!(*base, Expr::Ident(name) if name.value == "users"));
                }
                other => panic!("expected method inside row ref, got {other:?}"),
            },
            other => panic!("expected row ref, got {other:?}"),
        }
    }

    #[test]
    fn row_ref_logical_and() {
        match parse_ok("&a&&&b") {
            Expr::Binary { op, lhs, rhs, .. } => {
                assert_eq!(op.value, BinaryOp::And);
                assert!(matches!(*lhs, Expr::RowRef { .. }));
                assert!(matches!(*rhs, Expr::RowRef { .. }));
            }
            other => panic!("expected binary, got {other:?}"),
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
                            Expr::Cast { ty, .. } => {
                                assert_eq!(ty.value, TypeExpr::Scalar(TypeName::Int))
                            }
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
        assert_eq!(file.statements.len(), 2);
        assert!(
            matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Insert)
        );
        assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(0)));
        assert!(
            diags
                .iter()
                .all(|d| d.severity != grove_types::Severity::Warning)
        );
    }

    #[test]
    fn mutation_all_args_preserved_opt_depth() {
        let (file, diags) = parse_query("users?.insert({name = \"A\"}, {name = \"B\"});");
        let file = file.expect("should parse");
        let Statement::Mutation(mutation) = &file.statements[0] else {
            panic!("expected mutation");
        };
        assert_eq!(mutation.opt_depth, 1);
        assert_eq!(mutation.args.len(), 2);
        assert!(diags.is_empty(), "{diags:?}");
    }

    #[test]
    fn mutation_delete_statement() {
        let src = "users[!active].delete();";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(
            matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Delete)
        );
        assert!(file.output().is_none());
    }

    #[test]
    fn mutation_update_statement() {
        let src = "users.update({name = \"Bob\"});";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(
            matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Update)
        );
        assert!(file.output().is_none());
    }

    #[test]
    fn discarded_query_warning() {
        let src = "users { name }; 0";
        let (file, diags) = parse_query(src);
        let file = file.expect("should parse");
        assert_eq!(file.statements.len(), 2);
        assert!(matches!(&file.statements[0], Statement::Expr(_)));
        assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(0)));
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
    fn trailing_semicolon_expression_is_output() {
        let (file, diags) = parse_query("users { name };");
        let file = file.expect("should parse");
        assert!(file.output().is_some());
        assert!(diags.is_empty());
    }

    #[test]
    fn let_statement() {
        let (file, diags) = parse_query("let x = 1; x");
        let file = file.expect("should parse");
        assert_eq!(file.statements.len(), 2);
        assert!(matches!(&file.statements[0], Statement::Let(let_stmt)
                if let_stmt.name.value == "x"
                    && matches!(&let_stmt.value, Expr::Literal(lit) if lit.value == Literal::Int(1))));
        assert!(matches!(file.output(), Some(Expr::Ident(name)) if name.value == "x"));
        assert!(
            diags
                .iter()
                .all(|d| d.severity != grove_types::Severity::Warning)
        );
    }

    #[test]
    fn let_chain() {
        let file = parse_query("let a = 1; let b = a; b")
            .0
            .expect("should parse");
        assert_eq!(file.statements.len(), 3);
        assert!(matches!(&file.statements[0], Statement::Let(_)));
        assert!(matches!(&file.statements[1], Statement::Let(_)));
        assert!(matches!(file.output(), Some(Expr::Ident(name)) if name.value == "b"));
    }

    #[test]
    fn let_backtick_name() {
        let file = parse_query("let `let` = 1; `let`").0.expect("should parse");
        assert!(
            matches!(&file.statements[0], Statement::Let(let_stmt) if let_stmt.name.value == "let")
        );
    }

    #[test]
    fn let_missing_equals() {
        let (file, diags) = parse_query("let x 1;");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0026"));
    }

    #[test]
    fn let_missing_name() {
        let (file, diags) = parse_query("let = 1;");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0029"));
    }

    #[test]
    fn let_missing_semicolon() {
        let (file, diags) = parse_query("let x = 1");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0039"));
    }

    #[test]
    fn let_mutation_value() {
        let (file, diags) = parse_query("let x = users.insert({name = \"Alice\"});");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0028"));
    }

    #[test]
    fn let_without_output_is_valid() {
        let (file, diags) = parse_query("let x = 1;");
        let file = file.expect("statement-only file should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(matches!(&file.statements[0], Statement::Let(_)));
        assert!(file.output().is_none());
        assert!(diags.is_empty());
    }

    #[test]
    fn let_trailing_input() {
        let (file, diags) = parse_query("let x = 1 foo;");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0002"));
    }

    #[test]
    fn multiple_statements() {
        let src = "users.insert({name = \"Alice\"}); users.insert({name = \"Bob\"}); 42";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 3);
        assert!(
            file.statements[..2]
                .iter()
                .all(|s| matches!(s, Statement::Mutation(_)))
        );
        assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(42)));
    }

    #[test]
    fn statement_and_discarded() {
        let src = "users.insert({name = \"Alice\"}); users { name }; 42";
        let file = parse_query(src).0.expect("should parse");
        assert_eq!(file.statements.len(), 3);
        assert!(matches!(&file.statements[0], Statement::Mutation(_)));
        assert!(matches!(&file.statements[1], Statement::Expr(_)));
        assert!(matches!(file.output(), Some(Expr::Literal(lit)) if lit.value == Literal::Int(42)));
    }

    #[test]
    fn mutation_without_output_is_valid() {
        let (file, diags) = parse_query("users.insert({name = \"Alice\"});");
        let file = file.expect("mutation-only file should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(matches!(&file.statements[0], Statement::Mutation(_)));
        assert!(file.output().is_none());
        assert!(diags.is_empty());
    }

    #[test]
    fn mutation_missing_semicolon() {
        let (file, diags) = parse_query("users.insert({name = \"Alice\"})");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0039"));
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
        assert!(file.is_none());
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
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0022"]);
    }

    #[test]
    fn sync_to_stops_at_semicolon() {
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
