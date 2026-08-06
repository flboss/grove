use crate::ast::{BuiltinType, ColumnMapping, Field, ListStorage, StructDef, TypeExpr};
use crate::error::SchemaParseError;
use crate::token::TokenKind;
use grove_types::{Span, Spanned};

use super::{PResult, Parser};

impl<'src> Parser<'src> {
    pub(super) fn parse_struct(&mut self) -> PResult<Spanned<StructDef>> {
        let name = match self.expect_ident() {
            Ok(name) => name,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedStructName { span: found.span });
                self.sync_to_statement_boundary();
                return Err(());
            }
        };

        if let Err(found) = self.expect(TokenKind::LBrace) {
            self.emit_error(SchemaParseError::ExpectedStructLBrace {
                span: found.span,
                name: name.value,
            });
            self.sync_to_statement_boundary();
            return Err(());
        }

        let mut fields: Vec<Field> = Vec::new();
        let mut field_spans: Vec<(String, Span)> = Vec::new();

        let rbrace = loop {
            match self.peek().value {
                TokenKind::RBrace => break self.advance(),
                TokenKind::Eof => {
                    let struct_span = Span {
                        start: name.span.start,
                        end: self.peek().span.start,
                    };
                    self.emit_error(SchemaParseError::UnclosedStruct { span: struct_span });
                    return Err(());
                }
                _ => self.parse_field(&name.value, &mut fields, &mut field_spans)?,
            }
        };

        Ok(Spanned {
            span: Span {
                start: name.span.start,
                end: rbrace.span.end,
            },
            value: StructDef { name, fields },
        })
    }

    fn parse_field(
        &mut self,
        struct_name: &str,
        fields: &mut Vec<Field>,
        field_spans: &mut Vec<(String, Span)>,
    ) -> PResult<()> {
        let name = match self.expect_ident() {
            Ok(name) => name,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedStructFieldName {
                    span: found.span,
                    struct_name: struct_name.to_string(),
                });
                return self.sync_to_field_or_statement_boundary();
            }
        };

        if let Err(found) = self.expect(TokenKind::Colon) {
            self.emit_error(SchemaParseError::ExpectedStructFieldColon {
                span: found.span,
                field: name.value,
                struct_name: struct_name.to_string(),
            });
            return self.sync_to_field_or_statement_boundary();
        }

        let mut exposed_type = match self.parse_type_expr(true) {
            Ok(ty) => ty,
            Err(()) => return self.sync_to_field_or_statement_boundary(),
        };

        let column = if self.peek().value == TokenKind::At {
            self.advance();
            match self.parse_column_mapping() {
                Ok(map) => Some(map),
                Err(()) => return self.sync_to_field_or_statement_boundary(),
            }
        } else {
            None
        };

        if self.peek().value == TokenKind::Via {
            self.advance();
            let storage = match self.parse_via_storage() {
                Ok(storage) => storage,
                Err(()) => return self.sync_to_field_or_statement_boundary(),
            };

            match &mut exposed_type {
                TypeExpr::List { via, .. } => *via = Some(storage),
                _ => self.emit_error(SchemaParseError::ExpectedListForVia {
                    span: exposed_type.span(),
                }),
            }
        }

        match field_spans.iter().find(|(f, _)| f == &name.value) {
            Some((_, previous)) => {
                self.emit_error(SchemaParseError::DuplicateStructField {
                    span: name.span,
                    struct_name: struct_name.to_string(),
                    field: name.value.clone(),
                    previous: *previous,
                });
            }
            None => {
                field_spans.push((name.value.clone(), name.span));
                fields.push(Field {
                    name,
                    exposed_type,
                    column,
                });
            }
        }

        match self.peek().value {
            TokenKind::Comma => {
                self.advance();
            }
            TokenKind::RBrace | TokenKind::Eof => {}
            _ => {
                let span = self.peek().span;
                self.emit_error(SchemaParseError::ExpectedStructCommaOrRBrace {
                    span,
                    struct_name: struct_name.to_string(),
                });
                return self.sync_to_field_or_statement_boundary();
            }
        }
        Ok(())
    }

    fn parse_column_mapping(&mut self) -> PResult<ColumnMapping> {
        if self.peek().value == TokenKind::LParen {
            self.advance();
            let mut columns = vec![self.parse_paren_column()?];
            loop {
                match self.peek().value {
                    TokenKind::RParen => {
                        self.advance();
                        break;
                    }
                    TokenKind::Comma => {
                        self.advance();
                        if self.peek().value == TokenKind::RParen {
                            self.advance();
                            break;
                        }
                        columns.push(self.parse_paren_column()?);
                    }
                    _ => {
                        let span = self.peek().span;
                        self.emit_error(SchemaParseError::ExpectedColumnCommaOrRParen { span });
                        return Err(());
                    }
                }
            }
            Ok(ColumnMapping::Multi(columns))
        } else {
            match self.expect_ident() {
                Ok(col) => Ok(ColumnMapping::Single(col)),
                Err(found) => {
                    self.emit_error(SchemaParseError::ExpectedColumns { span: found.span });
                    Err(())
                }
            }
        }
    }

    fn parse_paren_column(&mut self) -> PResult<Spanned<String>> {
        match self.expect_ident() {
            Ok(col) => Ok(col),
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedColumn { span: found.span });
                Err(())
            }
        }
    }

    fn parse_type_expr(&mut self, skip_via: bool) -> PResult<TypeExpr> {
        if self.peek().value == TokenKind::Question {
            let question = self.advance();
            let inner = self.parse_base_type_expr(skip_via)?;
            let span = Span {
                start: question.span.start,
                end: inner.span().end,
            };
            Ok(TypeExpr::Optional {
                inner: Box::new(inner),
                span,
            })
        } else {
            self.parse_base_type_expr(skip_via)
        }
    }

    fn parse_base_type_expr(&mut self, skip_via: bool) -> PResult<TypeExpr> {
        match &self.peek().value {
            TokenKind::Int
            | TokenKind::Float
            | TokenKind::Dec
            | TokenKind::String
            | TokenKind::Bool
            | TokenKind::Instant
            | TokenKind::Duration => {
                let tok = self.advance();
                let builtin = match tok.value {
                    TokenKind::Int => BuiltinType::Int,
                    TokenKind::Float => BuiltinType::Float,
                    TokenKind::Dec => BuiltinType::Dec,
                    TokenKind::String => BuiltinType::String,
                    TokenKind::Bool => BuiltinType::Bool,
                    TokenKind::Instant => BuiltinType::Instant,
                    TokenKind::Duration => BuiltinType::Duration,
                    _ => unreachable!(),
                };
                Ok(TypeExpr::Primitive(tok.map(|_| builtin)))
            }
            TokenKind::Ident(_) => {
                let tok = self.advance();
                let TokenKind::Ident(s) = tok.value else {
                    unreachable!()
                };
                Ok(TypeExpr::Struct(Spanned {
                    span: tok.span,
                    value: s,
                }))
            }
            TokenKind::List => self.parse_list_type(skip_via),
            TokenKind::Tuple => self.parse_tuple_type(),
            TokenKind::LParen => self.parse_paren_tuple(),
            _ => {
                let span = self.peek().span;
                self.emit_error(SchemaParseError::ExpectedType { span });
                Err(())
            }
        }
    }

    fn parse_list_type(&mut self, skip_via: bool) -> PResult<TypeExpr> {
        let kw_tok = self.advance();
        if let Err(found) = self.expect(TokenKind::LAngle) {
            self.emit_error(SchemaParseError::ExpectedListLAngle { span: found.span });
            return Err(());
        }
        let element = Box::new(self.parse_type_expr(false)?);
        let rangle = match self.expect(TokenKind::RAngle) {
            Ok(tok) => tok,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedListRAngle { span: found.span });
                return Err(());
            }
        };
        let via = if !skip_via && self.peek().value == TokenKind::Via {
            self.advance();
            Some(self.parse_via_storage()?)
        } else {
            None
        };

        Ok(TypeExpr::List {
            element,
            via,
            span: Span {
                start: kw_tok.span.start,
                end: rangle.span.end,
            },
        })
    }

    fn parse_via_storage(&mut self) -> PResult<ListStorage> {
        let table = match self.expect_ident() {
            Ok(table) => table,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedListViaTable { span: found.span });
                return Err(());
            }
        };
        if let Err(found) = self.expect(TokenKind::LBracket) {
            self.emit_error(SchemaParseError::ExpectedListViaLBracket { span: found.span });
            return Err(());
        }
        let key_col = match self.expect_ident() {
            Ok(key_col) => key_col,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedListViaKeyCol { span: found.span });
                return Err(());
            }
        };
        if let Err(found) = self.expect(TokenKind::Comma) {
            self.emit_error(SchemaParseError::ExpectedListViaComma { span: found.span });
            return Err(());
        }
        let value = self.parse_column_mapping()?;
        if let Err(found) = self.expect(TokenKind::RBracket) {
            self.emit_error(SchemaParseError::ExpectedListViaRBracket { span: found.span });
            return Err(());
        }
        Ok(ListStorage {
            table,
            key_col,
            value,
        })
    }

    fn parse_tuple_type(&mut self) -> PResult<TypeExpr> {
        let kw_tok = self.advance();
        if let Err(found) = self.expect(TokenKind::LAngle) {
            self.emit_error(SchemaParseError::ExpectedTupleLAngle { span: found.span });
            return Err(());
        }
        let mut elements = vec![self.parse_type_expr(false)?];
        let rangle = loop {
            match self.peek().value {
                TokenKind::RAngle => break self.advance(),

                TokenKind::Eof => {
                    let span = self.peek().span;
                    self.emit_error(SchemaParseError::ExpectedTupleCommaOrRAngle { span });
                    return Err(());
                }
                _ => {
                    if let Err(found) = self.expect(TokenKind::Comma) {
                        self.emit_error(SchemaParseError::ExpectedTupleCommaOrRAngle {
                            span: found.span,
                        });
                        return Err(());
                    }
                    elements.push(self.parse_type_expr(false)?);
                }
            }
        };
        Ok(TypeExpr::Tuple {
            elements,
            span: Span {
                start: kw_tok.span.start,
                end: rangle.span.end,
            },
        })
    }

    fn parse_paren_tuple(&mut self) -> PResult<TypeExpr> {
        let kw_tok = self.advance();
        let mut elements = vec![self.parse_type_expr(false)?];
        let rparen = loop {
            match self.peek().value {
                TokenKind::RParen => break self.advance(),
                TokenKind::Eof => {
                    let span = self.peek().span;
                    self.emit_error(SchemaParseError::ExpectedTupleCommaOrRParen { span });
                    return Err(());
                }
                _ => {
                    if let Err(found) = self.expect(TokenKind::Comma) {
                        self.emit_error(SchemaParseError::ExpectedTupleCommaOrRParen {
                            span: found.span,
                        });
                        return Err(());
                    }
                    elements.push(self.parse_type_expr(false)?);
                }
            }
        };
        Ok(TypeExpr::Tuple {
            elements,
            span: Span {
                start: kw_tok.span.start,
                end: rparen.span.end,
            },
        })
    }

    fn sync_to_field_or_statement_boundary(&mut self) -> PResult<()> {
        self.sync_to(&[
            TokenKind::Comma,
            TokenKind::RBrace,
            TokenKind::Semicolon,
            TokenKind::Config,
            TokenKind::Root,
            TokenKind::Struct,
            TokenKind::Rel,
            TokenKind::Eof,
        ]);
        match self.peek().value {
            TokenKind::Comma => {
                self.advance();
                Ok(())
            }
            TokenKind::RBrace => Ok(()),
            TokenKind::Semicolon => {
                self.advance();
                Err(())
            }
            _ => Err(()),
        }
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
    fn struct_with_fields() {
        let (schema, diags) = parse_schema(
            "struct User { name: String, age: Int, manager: ?User, orders: List<Order> }",
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let structs = schema.unwrap().structs;
        assert_eq!(structs.len(), 1);
        assert_eq!(structs[0].name.value, "User");
        assert_eq!(structs[0].fields.len(), 4);
        assert_eq!(structs[0].fields[0].name.value, "name");
        assert!(matches!(
            structs[0].fields[0].exposed_type,
            TypeExpr::Primitive(_)
        ));
        assert!(structs[0].fields[0].column.is_none());
        assert!(matches!(
            structs[0].fields[2].exposed_type,
            TypeExpr::Optional { .. }
        ));
        assert!(matches!(
            structs[0].fields[3].exposed_type,
            TypeExpr::List { via: None, .. }
        ));
    }

    #[test]
    fn struct_with_column_mapping() {
        let (schema, diags) = parse_schema("struct S { total: Float@order_total }");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let fields = schema.unwrap().structs[0].fields.clone();
        assert_eq!(fields[0].name.value, "total");
        match &fields[0].column {
            Some(ColumnMapping::Single(col)) => assert_eq!(col.value, "order_total"),
            _ => panic!("expected Single column mapping"),
        }
    }

    #[test]
    fn struct_with_tuple_and_paren_tuple_types() {
        let (schema, diags) =
            parse_schema("struct P { a: Tuple<Float, Float>, b: List<(Int, String)> }");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let fields = schema.unwrap().structs[0].fields.clone();
        assert!(matches!(fields[0].exposed_type, TypeExpr::Tuple { .. }));
        match fields[1].exposed_type.clone() {
            TypeExpr::List { element, via, .. } => {
                assert_eq!(via, None);
                assert!(matches!(*element, TypeExpr::Tuple { .. }));
            }
            _ => panic!("expected List<T> field type"),
        }
    }

    #[test]
    fn struct_trailing_comma_allowed() {
        let (schema, diags) = parse_schema("struct S { a: Int, }");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(schema.unwrap().structs[0].fields.len(), 1);
    }

    #[test]
    fn struct_empty_block() {
        let (schema, diags) = parse_schema("struct S {}");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert!(schema.unwrap().structs[0].fields.is_empty());
    }

    #[test]
    fn struct_multiple() {
        let (schema, diags) = parse_schema("struct A { x: Int } struct B { y: String }");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(schema.unwrap().structs.len(), 2);
    }

    #[test]
    fn struct_missing_name() {
        let (_, diags) = parse_schema("struct { a: Int }");
        assert_eq!(codes(&diags), vec!["SP0018"]);
    }

    #[test]
    fn struct_missing_lbrace() {
        let (_, diags) = parse_schema("struct S a: Int }");
        assert_eq!(codes(&diags), vec!["SP0019"]);
    }

    #[test]
    fn field_missing_colon() {
        let (_, diags) = parse_schema("struct S { a Int }");
        assert_eq!(codes(&diags), vec!["SP0021"]);
    }

    #[test]
    fn field_missing_name() {
        let (_, diags) = parse_schema("struct S { : Int }");
        assert_eq!(codes(&diags), vec!["SP0020"]);
    }

    #[test]
    fn field_bad_type() {
        let (_, diags) = parse_schema("struct S { a: }");
        assert_eq!(codes(&diags), vec!["SP0022"]);
    }

    #[test]
    fn field_at_missing_column() {
        let (_, diags) = parse_schema("struct S { a: Int@, b: Int }");
        assert_eq!(codes(&diags), vec!["SP0023"]);
    }

    #[test]
    fn struct_with_multi_column_mapping() {
        let (schema, diags) =
            parse_schema("struct S { rgb: Tuple<Int, Int, Int>@(red, green, blue) }");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        match &schema.unwrap().structs[0].fields[0].column {
            Some(ColumnMapping::Multi(cols)) => {
                let names: Vec<&str> = cols.iter().map(|c| c.as_str()).collect();
                assert_eq!(names, vec!["red", "green", "blue"]);
            }
            _ => panic!("expected Multi column mapping"),
        }
    }

    #[test]
    fn multi_column_empty_parens() {
        let (_, diags) = parse_schema("struct S { rgb: Tuple<Int, Int, Int>@() }");
        assert_eq!(codes(&diags), vec!["SP0033"]);
    }

    #[test]
    fn multi_column_trailing_comma() {
        let (_, diags) = parse_schema("struct S { rgb: Tuple<Int, Int, Int>@(red, ) }");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
    }

    #[test]
    fn array_field_with_via() {
        let (schema, diags) = parse_schema(
            "struct User { tags: List<String>@id via user_tags[user_key, tag_value] }",
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let field = schema.unwrap().structs[0].fields[0].clone();
        match &field.column {
            Some(ColumnMapping::Single(col)) => assert_eq!(col.value, "id"),
            _ => panic!("expected Single link column"),
        }
        match &field.exposed_type {
            TypeExpr::List { element, via, .. } => {
                assert!(matches!(**element, TypeExpr::Primitive(_)));
                let storage = via.as_ref().expect("expected via storage");
                assert_eq!(storage.table.value, "user_tags");
                assert_eq!(storage.key_col.value, "user_key");
                match &storage.value {
                    ColumnMapping::Single(col) => assert_eq!(col.value, "tag_value"),
                    _ => panic!("expected Single value column"),
                }
            }
            _ => panic!("expected List<...> field type"),
        }
    }

    #[test]
    fn array_field_via_multi_columns() {
        let (schema, diags) = parse_schema(
            "struct S { tags: List<String>@id via tag_table[user_key, (t_int, t_str)] }",
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let field = schema.unwrap().structs[0].fields[0].clone();
        match &field.exposed_type {
            TypeExpr::List { via, .. } => {
                let storage = via.as_ref().expect("expected via storage");
                match &storage.value {
                    ColumnMapping::Multi(cols) => {
                        let names: Vec<&str> = cols.iter().map(|c| c.as_str()).collect();
                        assert_eq!(names, vec!["t_int", "t_str"]);
                    }
                    _ => panic!("expected Multi value columns"),
                }
            }
            _ => panic!("expected List<...> field type"),
        }
    }

    #[test]
    fn nested_list_via() {
        let (schema, diags) = parse_schema(
            "struct S { tags: List<List<Int> via inner_table[ref, data]>@outer_ref via outer_table[ref, inner_ref] }",
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let field = schema.unwrap().structs[0].fields[0].clone();
        match &field.exposed_type {
            TypeExpr::List {
                element,
                via: outer,
                ..
            } => {
                match &field.column {
                    Some(ColumnMapping::Single(col)) => assert_eq!(col.value, "outer_ref"),
                    _ => panic!("expected Single outer link column"),
                }
                let outer = outer.as_ref().expect("expected outer via storage");
                assert_eq!(outer.table.value, "outer_table");
                match &**element {
                    TypeExpr::List {
                        element: inner,
                        via,
                        ..
                    } => {
                        assert!(matches!(**inner, TypeExpr::Primitive(_)));
                        let storage = via.as_ref().expect("expected inner via storage");
                        assert_eq!(storage.table.value, "inner_table");
                        assert_eq!(storage.key_col.value, "ref");
                        match &storage.value {
                            ColumnMapping::Single(col) => assert_eq!(col.value, "data"),
                            _ => panic!("expected Single inner value column"),
                        }
                    }
                    _ => panic!("expected outer List<inner List>"),
                }
            }
            _ => panic!("expected List<...> field type"),
        }
    }

    #[test]
    fn top_level_list_via_before_at() {
        let (_, diags) = parse_schema("struct S { tags: List<Int> via t[u, v] @col }");
        assert_eq!(codes(&diags), vec!["SP0029"]);
    }

    #[test]
    fn via_missing_table() {
        let (_, diags) = parse_schema("struct S { a: List<Int>@id via [u v] }");
        assert_eq!(codes(&diags), vec!["SP0035"]);
    }

    #[test]
    fn via_missing_lbracket() {
        let (_, diags) = parse_schema("struct S { a: List<Int>@id via t u }");
        assert_eq!(codes(&diags), vec!["SP0036"]);
    }

    #[test]
    fn via_missing_key_col() {
        let (_, diags) = parse_schema("struct S { a: List<Int>@id via t[, v] }");
        assert_eq!(codes(&diags), vec!["SP0037"]);
    }

    #[test]
    fn via_missing_comma() {
        let (_, diags) = parse_schema("struct S { a: List<Int>@id via t[u v] }");
        assert_eq!(codes(&diags), vec!["SP0038"]);
    }

    #[test]
    fn via_missing_rbracket() {
        let (_, diags) = parse_schema("struct S { a: List<Int>@id via t[u, v");
        assert_eq!(codes(&diags), vec!["SP0039"]);
    }

    #[test]
    fn via_on_non_list_type() {
        let (_, diags) = parse_schema("struct S { a: Int@x via t[u, v] }");
        assert_eq!(codes(&diags), vec!["SP0040"]);
    }

    #[test]
    fn multi_column_unclosed() {
        let (_, diags) = parse_schema("struct S { rgb: Tuple<Int, Int, Int>@(red, green");
        assert_eq!(codes(&diags), vec!["SP0034"]);
    }

    #[test]
    fn multi_column_missing_comma() {
        let (_, diags) = parse_schema("struct S { rgb: Tuple<Int, Int, Int>@(red green) }");
        assert_eq!(codes(&diags), vec!["SP0034"]);
    }

    #[test]
    fn duplicate_field() {
        let (_, diags) = parse_schema("struct S { a: Int, a: String }");
        assert_eq!(codes(&diags), vec!["SP0030"]);
        assert_eq!(diags[0].labels.len(), 2);
    }

    #[test]
    fn unclosed_struct() {
        let (_, diags) = parse_schema("struct S { a: Int,");
        assert_eq!(codes(&diags), vec!["SP0031"]);
    }

    #[test]
    fn duplicate_struct() {
        let (_, diags) = parse_schema("struct S {} struct S {}");
        assert_eq!(codes(&diags), vec!["SP0032"]);
        assert_eq!(diags[0].labels.len(), 2);
    }
}
