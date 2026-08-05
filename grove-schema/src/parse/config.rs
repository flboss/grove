use crate::ast::{ConfigBlock, DecArithmetic, IntArithmetic};
use crate::error::SchemaParseError;
use crate::token::{Token, TokenKind};
use grove_types::{Span, Spanned};

use super::{PResult, Parser};

const CONFIG_BLOCK_KEYS: &[&str] = &["int_arithmetic", "float_checks", "dec_arithmetic"];
const HELP_INT_ARITHMETIC_VALUES: &str = r#"`"checked"`, `"saturating"`, or `"wrapping"`"#;
const HELP_DEC_ARITHMETIC_VALUES: &str = r#"`"checked"` or `"saturating"`"#;
const HELP_FLOAT_CHECKS_VALUES: &str = "`true` or `false`";

impl<'src> Parser<'src> {
    pub(super) fn parse_config(&mut self) -> PResult<Spanned<ConfigBlock>> {
        let lbrace = match self.expect(TokenKind::LBrace) {
            Ok(tok) => tok,
            Err(found) => {
                self.emit_error(SchemaParseError::ExpectedConfigLBrace { span: found.span });
                self.sync_to_statement_boundary();
                return Err(());
            }
        };

        let mut block = ConfigBlock {
            int_arithmetic: None,
            float_checks: None,
            dec_arithmetic: None,
        };

        let rbrace = loop {
            match self.peek().value {
                TokenKind::RBrace => break self.advance(),
                TokenKind::Eof => {
                    let block_span = Span {
                        start: lbrace.span.start,
                        end: self.peek().span.start,
                    };
                    self.emit_error(SchemaParseError::UnclosedConfig { span: block_span });
                    return Err(());
                }
                _ => self.parse_config_entry(&mut block)?,
            }
        };

        // TODO: warning if empty block

        Ok(Spanned {
            span: Span {
                start: lbrace.span.start,
                end: rbrace.span.end,
            },
            value: block,
        })
    }

    fn parse_config_entry(&mut self, block: &mut ConfigBlock) -> PResult<()> {
        let key_tok = match self.peek().value {
            TokenKind::Ident(_) => self.advance(),
            _ => {
                let span = self.peek().span;
                self.emit_error(SchemaParseError::ExpectedConfigKey { span });
                return self.sync_to_config_key_or_statement_boundary();
            }
        };
        let TokenKind::Ident(key) = key_tok.value else {
            unreachable!()
        };

        if !CONFIG_BLOCK_KEYS.contains(&key.as_str()) {
            self.emit_error(SchemaParseError::UnknownConfigKey {
                span: key_tok.span,
                key,
            });
            return self.sync_to_config_key_or_statement_boundary();
        }

        if let Err(found) = self.expect(TokenKind::Equals) {
            self.emit_error(SchemaParseError::ExpectedConfigEquals {
                span: found.span,
                key,
            });
            return self.sync_to_config_key_or_statement_boundary();
        }

        let Ok(value_tok) = self.config_value_token(&key) else {
            return self.sync_to_config_key_or_statement_boundary();
        };

        match key.as_str() {
            "int_arithmetic" => self.parse_int_arithmetic(block, value_tok),
            "float_checks" => self.parse_float_checks(block, value_tok),
            "dec_arithmetic" => self.parse_dec_arithmetic(block, value_tok),
            _ => unreachable!(),
        }

        match self.peek().value {
            TokenKind::Comma => {
                self.advance();
            }
            TokenKind::RBrace | TokenKind::Eof => {}
            _ => {
                let span = self.peek().span;
                self.emit_error(SchemaParseError::ExpectedConfigCommaOrRBrace { span, key });
                return self.sync_to_config_key_or_statement_boundary();
            }
        }
        Ok(())
    }

    fn config_value_token(&mut self, key: &str) -> PResult<Token> {
        match self.peek().value {
            TokenKind::Comma | TokenKind::RBrace | TokenKind::Eof => {
                let span = self.peek().span;
                self.emit_error(SchemaParseError::ExpectedConfigValue {
                    span,
                    key: key.to_string(),
                });
                Err(())
            }
            _ => Ok(self.advance()),
        }
    }

    fn parse_int_arithmetic(&mut self, block: &mut ConfigBlock, tok: Token) {
        match &tok.value {
            TokenKind::StringLit(s) => match IntArithmetic::try_from(s.as_str()) {
                Ok(mode) => {
                    self.assign_config_or_duplicate(
                        &mut block.int_arithmetic,
                        tok,
                        mode,
                        "int_arithmetic",
                    );
                }
                Err(()) => {
                    self.invalid_config_value(
                        tok.span,
                        "int_arithmetic",
                        s.clone(),
                        HELP_INT_ARITHMETIC_VALUES,
                    );
                }
            },
            _ => {
                self.invalid_config_value(
                    tok.span,
                    "int_arithmetic",
                    super::token_text(&tok),
                    HELP_INT_ARITHMETIC_VALUES,
                );
            }
        };
    }

    fn parse_dec_arithmetic(&mut self, block: &mut ConfigBlock, tok: Token) {
        match &tok.value {
            TokenKind::StringLit(s) => match DecArithmetic::try_from(s.as_str()) {
                Ok(mode) => self.assign_config_or_duplicate(
                    &mut block.dec_arithmetic,
                    tok,
                    mode,
                    "dec_arithmetic",
                ),

                Err(()) => {
                    self.invalid_config_value(
                        tok.span,
                        "dec_arithmetic",
                        s.clone(),
                        HELP_DEC_ARITHMETIC_VALUES,
                    );
                }
            },
            _ => {
                self.invalid_config_value(
                    tok.span,
                    "dec_arithmetic",
                    super::token_text(&tok),
                    HELP_DEC_ARITHMETIC_VALUES,
                );
            }
        };
    }

    fn parse_float_checks(&mut self, block: &mut ConfigBlock, tok: Token) {
        let checks = match &tok.value {
            TokenKind::Ident(s) => match s.as_str() {
                "true" => Some(true),
                "false" => Some(false),
                _ => {
                    self.invalid_config_value(
                        tok.span,
                        "float_checks",
                        s.clone(),
                        HELP_FLOAT_CHECKS_VALUES,
                    );
                    None
                }
            },
            _ => {
                self.invalid_config_value(
                    tok.span,
                    "float_checks",
                    super::token_text(&tok),
                    HELP_FLOAT_CHECKS_VALUES,
                );
                None
            }
        };
        if let Some(checks) = checks {
            self.assign_config_or_duplicate(&mut block.float_checks, tok, checks, "float_checks");
        }
    }

    fn invalid_config_value(&mut self, span: Span, key: &str, value: String, valid: &str) {
        self.emit_error(SchemaParseError::InvalidConfigValue {
            span,
            key: key.to_string(),
            value,
            valid: valid.to_string(),
        });
    }

    fn assign_config_or_duplicate<T>(
        &mut self,
        slot: &mut Option<Spanned<T>>,
        tok: Token,
        value: T,
        key: &str,
    ) {
        if let Some(previous) = slot {
            self.emit_error(SchemaParseError::DuplicateConfigKey {
                span: tok.span,
                key: key.to_string(),
                previous: previous.span,
            });
        } else {
            *slot = Some(Spanned {
                span: tok.span,
                value,
            });
        }
    }

    fn sync_to_config_key_or_statement_boundary(&mut self) -> PResult<()> {
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
    fn empty_input_has_no_config() {
        let (schema, diags) = parse_schema("");
        assert!(diags.is_empty());
        assert!(schema.unwrap().config.is_none());
    }

    #[test]
    fn config_all_keys() {
        let (schema, diags) = parse_schema(
            r#"config {
                int_arithmetic = "saturating",
                float_checks = false,
                dec_arithmetic = "checked",
            }"#,
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let config = schema.unwrap().config.unwrap();
        assert_eq!(
            config.int_arithmetic.as_ref().unwrap().value,
            IntArithmetic::Saturating
        );
        assert!(!config.float_checks.as_ref().unwrap().value);
        assert_eq!(
            config.dec_arithmetic.as_ref().unwrap().value,
            DecArithmetic::Checked
        );
    }

    #[test]
    fn config_subset() {
        let (schema, diags) = parse_schema(r#"config { int_arithmetic = "wrapping" }"#);
        assert!(diags.is_empty());
        let config = schema.unwrap().config.unwrap();
        assert_eq!(
            config.int_arithmetic.as_ref().unwrap().value,
            IntArithmetic::Wrapping
        );
        assert!(config.float_checks.is_none());
        assert!(config.dec_arithmetic.is_none());
    }

    #[test]
    fn config_empty_block() {
        let (schema, diags) = parse_schema("config{}");
        assert!(diags.is_empty());
        assert!(schema.unwrap().config.is_some());
    }

    #[test]
    fn all_int_arithmetic_modes() {
        for (value, expected) in [
            ("checked", IntArithmetic::Checked),
            ("saturating", IntArithmetic::Saturating),
            ("wrapping", IntArithmetic::Wrapping),
        ] {
            let src = format!(r#"config {{ int_arithmetic = "{value}" }}"#);
            let (schema, diags) = parse_schema(&src);
            assert!(
                diags.is_empty(),
                "unexpected diagnostics for {value}: {diags:?}"
            );
            assert_eq!(
                schema
                    .unwrap()
                    .config
                    .unwrap()
                    .int_arithmetic
                    .as_ref()
                    .unwrap()
                    .value,
                expected,
            );
        }
    }

    #[test]
    fn all_dec_arithmetic_modes() {
        for (value, expected) in [
            ("checked", DecArithmetic::Checked),
            ("saturating", DecArithmetic::Saturating),
        ] {
            let src = format!(r#"config {{ dec_arithmetic = "{value}" }}"#);
            let (schema, diags) = parse_schema(&src);
            assert!(
                diags.is_empty(),
                "unexpected diagnostics for {value}: {diags:?}"
            );
            assert_eq!(
                schema
                    .unwrap()
                    .config
                    .unwrap()
                    .dec_arithmetic
                    .as_ref()
                    .unwrap()
                    .value,
                expected,
            );
        }
    }

    #[test]
    fn all_float_checks_values() {
        for (value, expected) in [("true", true), ("false", false)] {
            let src = format!(r#"config {{ float_checks = {value} }}"#);
            let (schema, diags) = parse_schema(&src);
            assert!(
                diags.is_empty(),
                "unexpected diagnostics for {value}: {diags:?}"
            );
            assert_eq!(
                schema
                    .unwrap()
                    .config
                    .unwrap()
                    .float_checks
                    .as_ref()
                    .unwrap()
                    .value,
                expected,
            );
        }
    }

    #[test]
    fn config_trailing_comma_allowed() {
        let (schema, diags) = parse_schema(r#"config { int_arithmetic = "checked", }"#);
        assert!(diags.is_empty());
        assert!(schema.unwrap().config.is_some());
    }

    #[test]
    fn stray_comma_is_error() {
        let (schema, diags) = parse_schema(r#"config { , int_arithmetic = "checked" }"#);
        assert_eq!(codes(&diags), vec!["SP0003"]);
        assert!(schema.is_none(), "returned faulty schema");
    }

    #[test]
    fn comments_between_config_entries() {
        let (schema, diags) = parse_schema(
            r#"config {
                int_arithmetic = "checked",
                # a comment
                float_checks = true,
            }"#,
        );
        assert!(diags.is_empty(), "unexpected diagnostics: {diags:?}");
        let config = schema.unwrap().config.unwrap();
        assert_eq!(
            config.int_arithmetic.as_ref().unwrap().value,
            IntArithmetic::Checked
        );
        assert!(config.float_checks.as_ref().unwrap().value);
    }

    #[test]
    fn unknown_config_key_skips_entry() {
        let (_, diags) = parse_schema(r#"config { bogus = "x", int_arithmetic = "wrapping" }"#);
        assert_eq!(codes(&diags), vec!["SP0004"]);
    }

    #[test]
    fn duplicate_config_key_rejected() {
        let (_, diags) =
            parse_schema(r#"config { int_arithmetic = "checked", int_arithmetic = "wrapping" }"#);
        assert_eq!(codes(&diags), vec!["SP0008"]);
        assert_eq!(diags[0].labels.len(), 2);
    }

    #[test]
    fn invalid_int_arithmetic_value() {
        let (_, diags) = parse_schema(r#"config { int_arithmetic = "bogus" }"#);
        assert_eq!(codes(&diags), vec!["SP0007"]);
        assert_eq!(diags[0].labels[0].span, Span { start: 26, end: 33 });
    }

    #[test]
    fn float_checks_rejects_string() {
        let (_, diags) = parse_schema(r#"config { float_checks = "true" }"#);
        assert_eq!(codes(&diags), vec!["SP0007"]);
    }

    #[test]
    fn config_missing_equals() {
        let (_, diags) = parse_schema(r#"config { int_arithmetic "checked" }"#);
        assert_eq!(codes(&diags), vec!["SP0005"]);
    }

    #[test]
    fn config_missing_value() {
        let (_, diags) = parse_schema(r#"config { int_arithmetic = }"#);
        assert_eq!(codes(&diags), vec!["SP0006"]);
    }

    #[test]
    fn unclosed_config() {
        let (_, diags) = parse_schema(r#"config { int_arithmetic = "checked""#);
        assert_eq!(codes(&diags), vec!["SP0010"]);
    }

    #[test]
    fn config_missing_lbrace() {
        let (_, diags) = parse_schema("config int_arithmetic");
        assert_eq!(codes(&diags), vec!["SP0002"]);
    }

    #[test]
    fn config_key_must_be_ident() {
        let (_, diags) = parse_schema(r#"config { "a" = "x" }"#);
        assert_eq!(codes(&diags), vec!["SP0003"]);
    }

    #[test]
    fn config_entry_recovers_to_next_key() {
        let (_, diags) = parse_schema(
            r#"config {
                int_arithmetic "x",
                float_checks = true,
                dec_arithmetic = "invalid"
            }"#,
        );
        assert_eq!(codes(&diags), vec!["SP0005", "SP0007"]);
    }
}
