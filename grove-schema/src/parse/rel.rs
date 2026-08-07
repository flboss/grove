use crate::ast::{ColumnRef, FkMapping, JoinColumn, Relation, RelationArrow, RelationEndpoint};
use crate::error::SchemaParseError;
use crate::token::TokenKind;
use grove_types::{Span, Spanned};

use super::{PResult, Parser};

impl<'src> Parser<'src> {
    pub(super) fn parse_rel(&mut self) -> PResult<Relation> {
        let child = self.parse_endpoint()?;
        let arrow = self.parse_arrow()?;
        let parent = self.parse_endpoint()?;

        let fk = if self.peek().value == TokenKind::Via {
            self.advance();
            self.parse_fk_indirect()?
        } else {
            self.parse_fk_direct()?
        };

        self.expect_or_sync(TokenKind::Semicolon, |span| {
            SchemaParseError::ExpectedRelSemicolon { span }
        })?;

        Ok(Relation {
            child,
            arrow,
            parent,
            fk,
        })
    }

    fn parse_endpoint(&mut self) -> PResult<RelationEndpoint> {
        let struct_name =
            self.expect_ident_or_sync(|span| SchemaParseError::ExpectedRelEndpointStruct { span })?;
        self.expect_or_sync(TokenKind::Dot, |span| {
            SchemaParseError::ExpectedRelEndpointDot { span }
        })?;
        let field_name =
            self.expect_ident_or_sync(|span| SchemaParseError::ExpectedRelEndpointField { span })?;
        Ok(RelationEndpoint {
            struct_name,
            field_name,
        })
    }

    fn parse_arrow(&mut self) -> PResult<Spanned<RelationArrow>> {
        let token = self.advance();
        let arrow = match token.value {
            TokenKind::OneToOne => RelationArrow::OneToOne,
            TokenKind::ManyToOne => RelationArrow::ManyToOne,
            TokenKind::OneToMany => RelationArrow::OneToMany,
            TokenKind::ManyToMany => RelationArrow::ManyToMany,
            _ => {
                self.emit_error(SchemaParseError::ExpectedRelArrow { span: token.span });
                self.sync_to_statement_boundary();
                return Err(());
            }
        };
        Ok(token.map(|_| arrow))
    }

    fn parse_fk_direct(&mut self) -> PResult<FkMapping> {
        self.expect_or_sync(TokenKind::LParen, |span| {
            SchemaParseError::ExpectedRelLParen { span }
        })?;

        let child = self.parse_column_ref(
            |span| SchemaParseError::ExpectedRelLinkTable { span },
            |span| SchemaParseError::ExpectedRelLinkDot { span },
            |span| SchemaParseError::ExpectedRelLinkColumn { span },
        )?;

        self.expect_or_sync(TokenKind::Arrow, |span| {
            SchemaParseError::ExpectedRelLinkArrow { span }
        })?;

        let parent = self.parse_column_ref(
            |span| SchemaParseError::ExpectedRelPKTable { span },
            |span| SchemaParseError::ExpectedRelPKDot { span },
            |span| SchemaParseError::ExpectedRelPKColumn { span },
        )?;

        self.expect_or_sync(TokenKind::RParen, |span| {
            SchemaParseError::ExpectedRelRParen { span }
        })?;

        Ok(FkMapping::Direct { child, parent })
    }

    fn parse_fk_indirect(&mut self) -> PResult<FkMapping> {
        let join_table =
            self.expect_ident_or_sync(|span| SchemaParseError::ExpectedRelViaTable { span })?;

        self.expect_or_sync(TokenKind::LBracket, |span| {
            SchemaParseError::ExpectedRelViaLBracket { span }
        })?;

        let a = self.parse_join_table_column()?;

        self.expect_or_sync(TokenKind::Comma, |span| {
            SchemaParseError::ExpectedRelViaCommaOrRBracket { span }
        })?;

        let b = self.parse_join_table_column()?;

        self.expect_or_sync(TokenKind::RBracket, |span| {
            SchemaParseError::ExpectedRelViaCommaOrRBracket { span }
        })?;

        Ok(FkMapping::Indirect { join_table, a, b })
    }

    fn parse_join_table_column(&mut self) -> PResult<JoinColumn> {
        let join_col =
            self.expect_ident_or_sync(|span| SchemaParseError::ExpectedRelViaColumn { span })?;

        self.expect_or_sync(TokenKind::Arrow, |span| {
            SchemaParseError::ExpectedRelViaArrow { span }
        })?;

        let target = self.parse_column_ref(
            |span| SchemaParseError::ExpectedRelViaTargetTable { span },
            |span| SchemaParseError::ExpectedRelViaTargetDot { span },
            |span| SchemaParseError::ExpectedRelViaTargetColumn { span },
        )?;

        Ok(JoinColumn { join_col, target })
    }

    fn parse_column_ref(
        &mut self,
        table_err: impl FnOnce(Span) -> SchemaParseError,
        dot_err: impl FnOnce(Span) -> SchemaParseError,
        col_err: impl FnOnce(Span) -> SchemaParseError,
    ) -> PResult<ColumnRef> {
        let table = self.expect_ident_or_sync(table_err)?;
        self.expect_or_sync(TokenKind::Dot, dot_err)?;
        let col = self.expect_ident_or_sync(col_err)?;
        Ok(ColumnRef { table, col })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_schema;
    use grove_types::Diagnostic;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    #[test]
    fn rel_one_to_one() {
        let (schema, diags) =
            parse_schema("rel Profile.user <-> User.profile (profiles.user_id -> users.id);");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let rel = &schema.unwrap().relations[0];
        assert_eq!(rel.child.struct_name.value, "Profile");
        assert_eq!(rel.child.field_name.value, "user");
        assert_eq!(rel.arrow.value, RelationArrow::OneToOne);
        assert_eq!(rel.parent.struct_name.value, "User");
        assert_eq!(rel.parent.field_name.value, "profile");
        match &rel.fk {
            FkMapping::Direct { child, parent } => {
                assert_eq!(child.table.value, "profiles");
                assert_eq!(child.col.value, "user_id");
                assert_eq!(parent.table.value, "users");
                assert_eq!(parent.col.value, "id");
            }
            _ => panic!("expected Direct FK mapping"),
        }
    }

    #[test]
    fn rel_many_to_one() {
        let (schema, diags) =
            parse_schema("rel Order.user <<-> User.orders (orders.user_id -> users.id);");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let rel = &schema.unwrap().relations[0];
        assert_eq!(rel.child.struct_name.value, "Order");
        assert_eq!(rel.arrow.value, RelationArrow::ManyToOne);
        assert_eq!(rel.parent.struct_name.value, "User");
        match &rel.fk {
            FkMapping::Direct { child, parent } => {
                assert_eq!(child.table.value, "orders");
                assert_eq!(child.col.value, "user_id");
                assert_eq!(parent.col.value, "id");
            }
            _ => panic!("expected Direct FK mapping"),
        }
    }

    #[test]
    fn rel_many_to_many() {
        let (schema, diags) = parse_schema(
            "rel Role.users <<->> User.roles via user_roles[role_id -> roles.id, user_id -> users.id];",
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let rel = &schema.unwrap().relations[0];
        assert_eq!(rel.child.struct_name.value, "Role");
        assert_eq!(rel.arrow.value, RelationArrow::ManyToMany);
        assert_eq!(rel.parent.struct_name.value, "User");
        match &rel.fk {
            FkMapping::Indirect { join_table, a, b } => {
                assert_eq!(join_table.value, "user_roles");
                assert_eq!(a.join_col.value, "role_id");
                assert_eq!(a.target.table.value, "roles");
                assert_eq!(a.target.col.value, "id");
                assert_eq!(b.join_col.value, "user_id");
                assert_eq!(b.target.table.value, "users");
                assert_eq!(b.target.col.value, "id");
            }
            _ => panic!("expected Indirect FK mapping"),
        }
    }

    #[test]
    fn rel_self_reference() {
        let (schema, diags) =
            parse_schema("rel User.manager <<-> User.subordinates (users.manager_id -> users.id);");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let rel = &schema.unwrap().relations[0];
        assert_eq!(rel.child.struct_name.value, "User");
        assert_eq!(rel.arrow.value, RelationArrow::ManyToOne);
        assert_eq!(rel.parent.struct_name.value, "User");
        assert_eq!(rel.parent.field_name.value, "subordinates");
    }

    #[test]
    fn rel_missing_endpoint_struct() {
        let (_, diags) = parse_schema("rel .user <-> User.profile (profiles.user_id -> users.id);");
        assert_eq!(codes(&diags), vec!["SP0041"]);
    }

    #[test]
    fn rel_missing_endpoint_dot() {
        let (_, diags) = parse_schema("rel X <-> Y.y (a.a -> b.b);");
        assert_eq!(codes(&diags), vec!["SP0042"]);
    }

    #[test]
    fn rel_missing_endpoint_field() {
        let (_, diags) = parse_schema("rel X. <-> Y.y (a.a -> b.b);");
        assert_eq!(codes(&diags), vec!["SP0043"]);
    }

    #[test]
    fn rel_missing_arrow() {
        let (_, diags) = parse_schema("rel X.x Y.y (a.a -> b.b);");
        assert_eq!(codes(&diags), vec!["SP0044"]);
    }

    #[test]
    fn rel_missing_semicolon() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (a.a -> b.b)");
        assert_eq!(codes(&diags), vec!["SP0045"]);
    }

    #[test]
    fn rel_missing_lparen() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y a.a -> b.b;");
        assert_eq!(codes(&diags), vec!["SP0046"]);
    }

    #[test]
    fn rel_fk_missing_table() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (.a -> b.b);");
        assert_eq!(codes(&diags), vec!["SP0047"]);
    }

    #[test]
    fn rel_fk_missing_dot() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (ab -> b.b);");
        assert_eq!(codes(&diags), vec!["SP0048"]);
    }

    #[test]
    fn rel_fk_missing_column() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (a. -> b.b);");
        assert_eq!(codes(&diags), vec!["SP0049"]);
    }

    #[test]
    fn rel_fk_missing_arrow() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (a.a b.b);");
        assert_eq!(codes(&diags), vec!["SP0050"]);
    }

    #[test]
    fn rel_pk_missing_table() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (a.a -> .b);");
        assert_eq!(codes(&diags), vec!["SP0051"]);
    }

    #[test]
    fn rel_pk_missing_dot() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (a.a -> bb);");
        assert_eq!(codes(&diags), vec!["SP0052"]);
    }

    #[test]
    fn rel_pk_missing_column() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (a.a -> b.);");
        assert_eq!(codes(&diags), vec!["SP0053"]);
    }

    #[test]
    fn rel_fk_missing_rparen() {
        let (_, diags) = parse_schema("rel X.x <-> Y.y (a.a -> b.b");
        assert_eq!(codes(&diags), vec!["SP0054"]);
    }

    #[test]
    fn rel_via_missing_join_table() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via [a -> a.a, b -> b.b];");
        assert_eq!(codes(&diags), vec!["SP0055"]);
    }

    #[test]
    fn rel_via_missing_lbracket() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via jt a -> a.a;");
        assert_eq!(codes(&diags), vec!["SP0056"]);
    }

    #[test]
    fn rel_via_missing_join_col() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via jt[ -> a.a, b -> b.b];");
        assert_eq!(codes(&diags), vec!["SP0057"]);
    }

    #[test]
    fn rel_via_missing_arrow() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via jt[a a.a, b -> b.b];");
        assert_eq!(codes(&diags), vec!["SP0058"]);
    }

    #[test]
    fn rel_via_missing_target_table() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via jt[a -> .a, b -> b.b];");
        assert_eq!(codes(&diags), vec!["SP0059"]);
    }

    #[test]
    fn rel_via_missing_target_dot() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via jt[a -> aa, b -> b.b];");
        assert_eq!(codes(&diags), vec!["SP0060"]);
    }

    #[test]
    fn rel_via_missing_target_column() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via jt[a -> a., b -> b.b];");
        assert_eq!(codes(&diags), vec!["SP0061"]);
    }

    #[test]
    fn rel_via_missing_comma_or_rbracket() {
        let (_, diags) = parse_schema("rel X.x <<->> Y.y via jt[a -> a.a b -> b.b];");
        assert_eq!(codes(&diags), vec!["SP0062"]);
    }
}
