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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{Literal, TypeExpr, TypeName};
    use crate::parse::tests::{cast_type, codes, parse_ok};
    use crate::parse_query;

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
    fn unary_binary_precedence() {
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
    fn error_recovery_method_args() {
        let (_, diags) = parse_query("users.sort(asc , asc )");
        assert_eq!(codes(&diags), vec!["QP0001", "QP0001"]);
    }

    #[test]
    fn error_recovery_synchronization_comma() {
        let (_, diags) = parse_query("users.sort(name, , age)");
        assert_eq!(codes(&diags), vec!["QP0001"]);
    }
}
