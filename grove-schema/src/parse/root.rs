use crate::ast::RootCollection;
use crate::error::SchemaSyntaxError;
use crate::token::TokenKind;
use grove_types::Spanned;

use super::{PResult, Parser};

impl<'src> Parser<'src> {
    pub(super) fn parse_root(&mut self) -> PResult<Spanned<RootCollection>> {
        let name = match self.expect_ident() {
            Ok(name) => name,
            Err(found) => {
                self.emit_error(SchemaSyntaxError::ExpectedRootName { span: found.span });
                self.sync_to_statement_boundary();
                return Err(());
            }
        };

        if let Err(found) = self.expect(TokenKind::Colon) {
            self.emit_error(SchemaSyntaxError::ExpectedRootColon {
                span: found.span,
                name: name.value,
            });
            self.sync_to_statement_boundary();
            return Err(());
        }

        let struct_name = match self.expect_ident() {
            Ok(name) => name,
            Err(found) => {
                self.emit_error(SchemaSyntaxError::ExpectedRootStructName { span: found.span });
                self.sync_to_statement_boundary();
                return Err(());
            }
        };

        let table = if self.peek().value == TokenKind::At {
            self.advance();
            match self.expect_ident() {
                Ok(table) => Some(table),
                Err(found) => {
                    self.emit_error(SchemaSyntaxError::ExpectedRootUnderlyingTable {
                        span: found.span,
                    });
                    self.sync_to_statement_boundary();
                    return Err(());
                }
            }
        } else {
            None
        };

        let semicolon = match self.expect(TokenKind::Semicolon) {
            Ok(semi) => semi,
            Err(found) => {
                self.emit_error(SchemaSyntaxError::ExpectedRootSemicolon { span: found.span });
                self.sync_to_statement_boundary();
                return Err(());
            }
        };

        Ok(Spanned::new(
            name.span.merge(semicolon.span),
            RootCollection {
                name,
                table,
                struct_name,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use crate::parse_schema;
    use grove_types::Diagnostic;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    #[test]
    fn root_simple() {
        let (schema, diags) = parse_schema("root users: User;");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let roots = schema.unwrap().roots;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name.value, "users");
        assert!(roots[0].table.is_none());
        assert_eq!(roots[0].struct_name.value, "User");
    }

    #[test]
    fn root_with_table() {
        let (schema, diags) = parse_schema("root new_name: SomeStruct @old_table;");
        assert!(diags.is_empty());
        let roots = schema.unwrap().roots;
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].name.value, "new_name");
        assert_eq!(roots[0].table.as_ref().unwrap().value, "old_table");
        assert_eq!(roots[0].struct_name.value, "SomeStruct");
    }

    #[test]
    fn root_multiple() {
        let (schema, diags) = parse_schema("root a: A; root b: B@b_table;");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let roots = schema.unwrap().roots;
        assert_eq!(roots.len(), 2);
        assert_eq!(roots[0].name.value, "a");
        assert_eq!(roots[0].struct_name.value, "A");
        assert!(roots[0].table.is_none());
        assert_eq!(roots[1].name.value, "b");
        assert_eq!(roots[1].struct_name.value, "B");
        assert_eq!(roots[1].table.as_ref().unwrap().value, "b_table");
    }

    #[test]
    fn root_missing_name() {
        let (_, diags) = parse_schema("root : User;");
        assert_eq!(codes(&diags), vec!["SP0012"]);
    }

    #[test]
    fn root_missing_colon() {
        let (_, diags) = parse_schema("root users User;");
        assert_eq!(codes(&diags), vec!["SP0013"]);
    }

    #[test]
    fn root_missing_struct_name() {
        let (_, diags) = parse_schema("root users: ;");
        assert_eq!(codes(&diags), vec!["SP0014"]);
    }

    #[test]
    fn root_missing_underlying_table() {
        let (_, diags) = parse_schema("root users: User@;");
        assert_eq!(codes(&diags), vec!["SP0015"]);
    }

    #[test]
    fn root_missing_semicolon() {
        let (_, diags) = parse_schema("root users: User");
        assert_eq!(codes(&diags), vec!["SP0016"]);
    }
}
