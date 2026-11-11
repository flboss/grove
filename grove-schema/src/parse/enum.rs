use std::fmt;

use crate::ast::{EnumDef, EnumValue, EnumVariantDef};
use crate::error::SchemaParseError;
use crate::token::TokenKind;
use grove_types::{Span, Spanned};

use super::{PResult, Parser};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ValueKind {
    Int,
    String,
}

impl fmt::Display for ValueKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValueKind::Int => write!(f, "Int"),
            ValueKind::String => write!(f, "String"),
        }
    }
}

impl<'src> Parser<'src> {
    pub(super) fn parse_enum(&mut self) -> PResult<Spanned<EnumDef>> {
        let name = match self.expect_ident() {
            Ok(name) => name,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedEnumName { span: found.span });
                self.sync_to_statement_boundary();
                return Err(());
            }
        };

        if let Err(found) = self.expect(TokenKind::LBrace) {
            self.emit_error(SchemaParseError::ExpectedEnumLBrace {
                span: found.span,
                name: name.value,
            });
            self.sync_to_statement_boundary();
            return Err(());
        }

        let diagnostics_before = self.diagnostics.len();
        let mut variants: Vec<EnumVariantDef> = Vec::new();
        let mut variant_spans: Vec<(String, Span)> = Vec::new();
        let mut value_spans: Vec<(EnumValue, Span)> = Vec::new();
        let mut underlying: Option<ValueKind> = None;

        let rbrace = loop {
            match self.peek().value {
                TokenKind::RBrace => break self.advance(),
                TokenKind::Eof => {
                    let span = Span {
                        start: name.span.start,
                        end: self.peek().span.start,
                    };
                    self.emit_error(SchemaParseError::UnclosedEnum { span });
                    return Err(());
                }
                _ => self.parse_enum_variant(
                    &name.value,
                    &mut variants,
                    &mut variant_spans,
                    &mut value_spans,
                    &mut underlying,
                )?,
            }
        };

        let span = Span {
            start: name.span.start,
            end: rbrace.span.end,
        };
        if variants.is_empty() && self.diagnostics.len() == diagnostics_before {
            self.emit_error(SchemaParseError::EmptyEnum {
                span,
                name: name.value.clone(),
            });
        }

        Ok(Spanned {
            span,
            value: EnumDef { name, variants },
        })
    }

    fn parse_enum_variant(
        &mut self,
        enum_name: &str,
        variants: &mut Vec<EnumVariantDef>,
        variant_spans: &mut Vec<(String, Span)>,
        value_spans: &mut Vec<(EnumValue, Span)>,
        underlying: &mut Option<ValueKind>,
    ) -> PResult<()> {
        let name = match self.expect_ident() {
            Ok(name) => name,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedEnumVariantName {
                    span: found.span,
                    enum_name: enum_name.to_string(),
                });
                return self.sync_to_enum_variant_or_statement_boundary();
            }
        };

        if let Err(found) = self.expect(TokenKind::FatArrow) {
            self.emit_error(SchemaParseError::ExpectedEnumArrow {
                span: found.span,
                enum_name: enum_name.to_string(),
                variant: name.value,
            });
            return self.sync_to_enum_variant_or_statement_boundary();
        }

        let value_span = self.peek().span;
        let (value, kind) = match self.peek().value.clone() {
            TokenKind::StringLit(value) => {
                self.advance();
                (EnumValue::String(value), ValueKind::String)
            }
            TokenKind::IntLit(value) => {
                self.advance();
                (EnumValue::Int(value), ValueKind::Int)
            }
            _ => {
                let span = self.peek().span;
                self.emit_error(SchemaParseError::ExpectedEnumValue {
                    span,
                    enum_name: enum_name.to_string(),
                    variant: name.value,
                });
                return self.sync_to_enum_variant_or_statement_boundary();
            }
        };

        if let Some((_, previous)) = variant_spans.iter().find(|(n, _)| n == &name.value) {
            self.emit_error(SchemaParseError::DuplicateEnumVariant {
                span: name.span,
                enum_name: enum_name.to_string(),
                variant: name.value,
                previous: *previous,
            });
            return self.sync_to_enum_variant_or_statement_boundary();
        }

        let duplicate_value = value_spans
            .iter()
            .find(|(v, _)| v == &value)
            .map(|(_, span)| *span);
        if let Some(previous) = duplicate_value {
            self.emit_error(SchemaParseError::DuplicateEnumValue {
                span: value_span,
                enum_name: enum_name.to_string(),
                value: value.to_string(),
                previous,
            });
        }

        match underlying {
            Some(expected) if *expected != kind => {
                self.emit_error(SchemaParseError::MixedEnumValueTypes {
                    span: value_span,
                    enum_name: enum_name.to_string(),
                    expected: expected.to_string(),
                    found: kind.to_string(),
                });
            }
            Some(_) => {}
            None => *underlying = Some(kind),
        }

        variant_spans.push((name.value.clone(), name.span));
        value_spans.push((value.clone(), value_span));
        variants.push(EnumVariantDef {
            name,
            value: Spanned {
                span: value_span,
                value,
            },
        });

        match self.peek().value {
            TokenKind::Comma => {
                self.advance();
                Ok(())
            }
            TokenKind::RBrace | TokenKind::Eof => Ok(()),
            _ => {
                let span = self.peek().span;
                self.emit_error(SchemaParseError::ExpectedEnumCommaOrRBrace {
                    span,
                    enum_name: enum_name.to_string(),
                });
                self.sync_to_enum_variant_or_statement_boundary()
            }
        }
    }

    fn sync_to_enum_variant_or_statement_boundary(&mut self) -> PResult<()> {
        self.sync_to(&[
            TokenKind::Comma,
            TokenKind::RBrace,
            TokenKind::Semicolon,
            TokenKind::Config,
            TokenKind::Root,
            TokenKind::Struct,
            TokenKind::Enum,
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
    use crate::ast::TypeExpr;
    use crate::parse_schema;
    use grove_types::Diagnostic;

    fn codes(diags: &[Diagnostic]) -> Vec<&str> {
        diags.iter().map(|d| d.code.as_ref()).collect()
    }

    #[test]
    fn enum_string_values() {
        let (schema, diags) =
            parse_schema(r#"enum Priority { Low => "low", Medium => "medium", High => "high" }"#);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let schema = schema.unwrap();
        assert_eq!(schema.enums.len(), 1);
        let priority = &schema.enums[0];
        assert_eq!(priority.name.value, "Priority");
        assert_eq!(
            priority
                .variants
                .iter()
                .map(|v| v.value.value.clone())
                .collect::<Vec<_>>(),
            vec![
                EnumValue::String("low".into()),
                EnumValue::String("medium".into()),
                EnumValue::String("high".into())
            ]
        );
    }

    #[test]
    fn enum_int_values_negative() {
        let (schema, diags) = parse_schema("enum Level { Low => -1, Medium => 0, High => 2 }");
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let level = &schema.as_ref().unwrap().enums[0];
        assert_eq!(
            level
                .variants
                .iter()
                .map(|v| v.value.value.clone())
                .collect::<Vec<_>>(),
            vec![EnumValue::Int(-1), EnumValue::Int(0), EnumValue::Int(2)]
        );
    }

    #[test]
    fn enum_trailing_comma() {
        let (schema, diags) = parse_schema(r#"enum P { A => "a", }"#);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        assert_eq!(schema.unwrap().enums[0].variants.len(), 1);
    }

    #[test]
    fn enum_as_named_field_type() {
        let (schema, diags) =
            parse_schema(r#"enum Priority { Low => "low" } struct Ticket { priority: Priority }"#);
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let ticket = &schema.unwrap().structs[0];
        match &ticket.fields[0].exposed_type {
            TypeExpr::Named(name) => assert_eq!(name.value, "Priority"),
            other => panic!("expected named type, got {other:?}"),
        }
    }

    #[test]
    fn enum_missing_name() {
        let (_, diags) = parse_schema("enum { A => 1 }");
        assert_eq!(codes(&diags), vec!["SP0063"]);
    }

    #[test]
    fn enum_missing_lbrace() {
        let (_, diags) = parse_schema("enum E A => 1 }");
        assert_eq!(codes(&diags), vec!["SP0064"]);
    }

    #[test]
    fn variant_missing_name() {
        let (_, diags) = parse_schema("enum E { => 1 }");
        assert_eq!(codes(&diags), vec!["SP0065"]);
    }

    #[test]
    fn variant_missing_arrow() {
        let (_, diags) = parse_schema("enum E { A 1 }");
        assert_eq!(codes(&diags), vec!["SP0066"]);
    }

    #[test]
    fn variant_missing_value() {
        let (_, diags) = parse_schema("enum E { A => }");
        assert_eq!(codes(&diags), vec!["SP0067"]);
    }

    #[test]
    fn variant_missing_comma() {
        let (_, diags) = parse_schema("enum E { A => 1 B => 2 }");
        assert_eq!(codes(&diags), vec!["SP0068"]);
    }

    #[test]
    fn unclosed_enum() {
        let (_, diags) = parse_schema("enum E { A => 1,");
        assert_eq!(codes(&diags), vec!["SP0069"]);
    }

    #[test]
    fn duplicate_variant() {
        let (_, diags) = parse_schema("enum E { A => 1, A => 2 }");
        assert_eq!(codes(&diags), vec!["SP0070"]);
        assert_eq!(diags[0].labels.len(), 2);
    }

    #[test]
    fn duplicate_value() {
        let (_, diags) = parse_schema("enum E { A => 1, B => 1 }");
        assert_eq!(codes(&diags), vec!["SP0071"]);
        assert_eq!(diags[0].labels.len(), 2);
    }

    #[test]
    fn mixed_value_types() {
        let (_, diags) = parse_schema(r#"enum E { A => 1, B => "b" }"#);
        assert_eq!(codes(&diags), vec!["SP0072"]);
    }

    #[test]
    fn duplicate_enum() {
        let (_, diags) = parse_schema("enum E { A => 1 } enum E { B => 2 }");
        assert_eq!(codes(&diags), vec!["SP0073"]);
        assert_eq!(diags[0].labels.len(), 2);
    }

    #[test]
    fn enum_struct_namespace_collision() {
        for source in [
            "struct S {} enum S { A => 1 }",
            "enum S { A => 1 } struct S {}",
        ] {
            let (_, diags) = parse_schema(source);
            assert_eq!(codes(&diags), vec!["SP0074"], "source: {source}");
            assert_eq!(diags[0].labels.len(), 2);
        }
    }

    #[test]
    fn empty_enum() {
        let (_, diags) = parse_schema("enum E {}");
        assert_eq!(codes(&diags), vec!["SP0075"]);
    }
}
