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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Literal;
    use crate::parse::tests::codes;
    use crate::parse_query;

    #[test]
    fn trailing_input() {
        let (file, diags) = parse_query("42 \"leftover\"");
        assert!(file.is_none());
        assert_eq!(codes(&diags), vec!["QP0002"]);
    }

    #[test]
    fn mutation_insert_statement() {
        let src = "users.insert({name = \"Alice\"});";
        let (file, diags) = parse_query(src);
        let file = file.expect("should parse");
        assert_eq!(file.statements.len(), 1);
        assert!(
            matches!(&file.statements[0], Statement::Mutation(m) if m.kind.value == MutationKind::Insert)
        );
        assert!(file.output().is_none());
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
    fn let_standalone() {
        let (file, diags) = parse_query("let x = 1;");
        let file = file.expect("should parse");
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
    fn mutation_missing_semicolon() {
        let (file, diags) = parse_query("users.insert({name = \"Alice\"})");
        assert!(file.is_none());
        assert!(diags.iter().any(|d| d.code.as_ref() == "QP0039"));
    }
}
