use crate::ast::{ConstantName, Expr, Literal, QueryFile, TypeName};
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

    fn at(&self, pred: impl Fn(&TokenKind) -> bool) -> bool {
        pred(&self.current.value)
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

    fn parse_file(&mut self) -> Result<QueryFile, ()> {
        let result = self.parse_expr()?;
        if !self.at(|k| matches!(k, TokenKind::Eof)) {
            self.emit_error(QueryParseError::TrailingInput {
                span: self.current.span,
            });
            return Err(());
        }
        Ok(QueryFile {
            statements: Vec::new(),
            result,
        })
    }

    fn parse_expr(&mut self) -> Result<Expr, ()> {
        self.parse_primary()
    }

    fn parse_primary(&mut self) -> Result<Expr, ()> {
        let kind = self.current.value.clone();
        let span = self.current.span;

        match kind {
            TokenKind::IntLit(i) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Int(i),
                }))
            }
            TokenKind::FloatLit(f) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Float(f),
                }))
            }
            TokenKind::DecLit(d) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Dec(d),
                }))
            }
            TokenKind::StringLit(s) => {
                self.bump();
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
            TokenKind::InstantLit(dt) => {
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
            TokenKind::Today(t) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Today(t),
                }))
            }
            TokenKind::DurationLit(d) => {
                self.bump();
                Ok(Expr::Literal(Spanned {
                    span,
                    value: Literal::Duration(d),
                }))
            }
            TokenKind::Ident(name) => {
                self.bump();
                Ok(Expr::Ident(Spanned { span, value: name }))
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
        let first = self.parse_expr()?;

        if !self.at(|k| matches!(k, TokenKind::Comma)) {
            self.expect(
                |k| matches!(k, TokenKind::RParen),
                |span| QueryParseError::TupleCommaOrRParenExpected { span },
            )?;
            return Ok(first);
        }

        let mut elements = vec![first];
        while self.at(|k| matches!(k, TokenKind::Comma)) {
            self.bump();
            if self.at(|k| matches!(k, TokenKind::RParen)) {
                break;
            }
            elements.push(self.parse_expr()?);
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
        if !self.at(|k| matches!(k, TokenKind::RBracket)) {
            loop {
                if self.at(|k| matches!(k, TokenKind::RBracket)) {
                    break;
                }
                elements.push(self.parse_expr()?);
                if self.at(|k| matches!(k, TokenKind::Comma)) {
                    self.bump();
                } else {
                    break;
                }
            }
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
        let value = self.parse_expr()?;
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
        let ty = self.bump();
        let ty_name = match ty.value {
            TokenKind::Int => TypeName::Int,
            TokenKind::Float => TypeName::Float,
            TokenKind::Dec => TypeName::Dec,
            _ => unreachable!("only type keywords dispatch here"),
        };

        if !self.at(|k| matches!(k, TokenKind::DoubleColon)) {
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
            ty: Spanned {
                span: ty.span,
                value: ty_name,
            },
            name: Spanned {
                span: self.current.span,
                value: constant,
            },
        })
    }

    fn parse_if_condition(&mut self) -> Result<Expr, ()> {
        if self.at(|k| matches!(k, TokenKind::LParen)) {
            self.bump();
            let cond = self.parse_expr()?;
            self.expect(
                |k| matches!(k, TokenKind::RParen),
                |span| QueryParseError::TupleCommaOrRParenExpected { span },
            )?;
            Ok(cond)
        } else {
            // TODO: restrict parsing of postfix projection syntax
            self.parse_expr()
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
        let value = self.parse_expr()?;
        let rbrace = self.expect(
            |k| matches!(k, TokenKind::RBrace),
            |span| QueryParseError::IfRBraceExpected { span },
        )?;

        let mut arms = vec![(cond, value)];
        let mut end = rbrace.span.end;
        let mut default: Option<Box<Expr>> = None;

        while self.at(|k| matches!(k, TokenKind::Else)) {
            self.bump();
            if self.at(|k| matches!(k, TokenKind::If)) {
                self.bump();
                let cond = self.parse_if_condition()?;
                self.expect(
                    |k| matches!(k, TokenKind::LBrace),
                    |span| QueryParseError::IfLBraceExpected { span },
                )?;
                let value = self.parse_expr()?;
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
                let value = self.parse_expr()?;
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
            let field_value = self.parse_expr()?;
            fields.push((name, field_value));

            if self.at(|k| matches!(k, TokenKind::Comma)) {
                self.bump();
                if self.at(|k| matches!(k, TokenKind::RBrace)) {
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

    fn emit_error(&mut self, err: QueryParseError) {
        self.diagnostics.push(err.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_query;
    use grove_types::Diagnostic;
    use rust_decimal::Decimal;

    fn codes(diagnostics: &[Diagnostic]) -> Vec<&str> {
        diagnostics.iter().map(|d| d.code.as_ref()).collect()
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
        assert_eq!(codes(&diags), vec!["QP0003"]);
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
}
