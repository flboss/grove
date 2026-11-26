use std::collections::HashMap;

use crate::ast::{TypeExpr, TypeName};
use crate::parse::Parser;
use crate::parse::error::QueryParseError;
use crate::token::{Token, TokenKind};
use grove_types::{Span, Spanned};

impl<'src> Parser<'src> {
    pub(super) fn parse_type_name(&mut self) -> Result<Spanned<TypeName>, Token> {
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

    pub(super) fn parse_type_expr(&mut self) -> Result<Spanned<TypeExpr>, ()> {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::tests::{cast_type, codes};
    use crate::parse_query;

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
}
