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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{BinaryOp, TypeName};
    use crate::parse::tests::{codes, parse_ok};
    use crate::parse_query;
    use rust_decimal::dec;

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
    fn if_condition_expr() {
        match parse_ok("if x == 0 { 1 } else { 2 }") {
            Expr::If { arms, .. } => {
                assert!(matches!(&arms[0].0, Expr::Binary { op, .. } if op.value == BinaryOp::Eq));
            }
            other => panic!("expected if, got {other:?}"),
        }
    }

    #[test]
    fn error_recovery_array_elements() {
        let (file, diags) = parse_query("[,]");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }
}
