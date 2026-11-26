use crate::lex::Lexer;
use crate::lex::error::QueryLexError;
use crate::token::{Token, TokenKind};

impl<'src> Lexer<'src> {
    pub(super) fn scan_backtick_ident(&mut self, start: usize) -> Token {
        let content_start = self.pos;
        loop {
            match self.peek_char() {
                None | Some('\n') => {
                    self.emit_error(QueryLexError::UnclosedBacktickIdent {
                        span: self.span_from(start),
                    });
                    let content = self.source[content_start..self.pos].to_string();
                    return self.make_token(start, TokenKind::Ident(content));
                }
                Some('`') => {
                    let content = self.source[content_start..self.pos].to_string();
                    self.bump();
                    return self.make_token(start, TokenKind::Ident(content));
                }
                Some(_) => self.bump(),
            }
        }
    }

    pub(super) fn scan_string(&mut self, start: usize) -> Token {
        let mut content = String::new();
        loop {
            match self.peek_char() {
                None | Some('\n') => {
                    self.emit_error(QueryLexError::UnclosedString {
                        span: self.span_from(start),
                    });
                    break;
                }
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.scan_escape(&mut content);
                }
                Some(ch) => {
                    self.bump();
                    content.push(ch);
                }
            }
        }
        self.make_token(start, TokenKind::StringLit(content))
    }

    fn scan_escape(&mut self, content: &mut String) {
        let start = self.pos;
        self.bump();
        let Some(ch) = self.peek_char() else {
            return;
        };
        match ch {
            'n' => {
                self.bump();
                content.push('\n');
            }
            't' => {
                self.bump();
                content.push('\t');
            }
            'r' => {
                self.bump();
                content.push('\r');
            }
            '\\' => {
                self.bump();
                content.push('\\');
            }
            '"' => {
                self.bump();
                content.push('"');
            }
            '0' => {
                self.bump();
                content.push('\0');
            }
            'x' => {
                self.bump();
                self.scan_hex_escape(content);
            }
            'u' => {
                self.bump();
                self.scan_unicode_escape(content);
            }
            _ => {
                self.bump();
                self.emit_error(QueryLexError::UnknownEscape {
                    span: self.span_from(start),
                    ch,
                });
            }
        }
    }

    fn scan_hex_escape(&mut self, content: &mut String) {
        let start = self.pos;
        let mut value = 0u32;
        for _ in 0..2 {
            match self.peek_char() {
                Some(d) if d.is_ascii_hexdigit() => {
                    value = value * 16 + d.to_digit(16).unwrap();
                    self.bump();
                }
                _ => {
                    self.emit_error(QueryLexError::HexEscapeInvalid {
                        span: self.span_from(start),
                    });
                    return;
                }
            }
        }
        if value >= 0x80 {
            self.emit_error(QueryLexError::HexEscapeNonAscii {
                span: self.span_from(start),
                value: value as u8,
            });
        } else {
            content.push(char::from_u32(value).unwrap());
        }
    }

    fn scan_unicode_escape(&mut self, content: &mut String) {
        let start = self.pos;

        match self.peek_char() {
            Some('{') => self.bump(),
            None | Some('\n') => return,
            Some(_) => {
                let span = self.span_from(start);
                self.emit_error(QueryLexError::UnicodeEscapeExpectedLBrace { span });
                return;
            }
        }

        let mut value = 0u32;
        let mut digits = 0u32;
        let mut invalid = false;
        loop {
            match self.peek_char() {
                Some('}') => {
                    self.bump();
                    break;
                }
                None | Some('\n') => return,
                Some(d) if d.is_ascii_hexdigit() => {
                    if digits < 6 {
                        value = value * 16 + d.to_digit(16).unwrap();
                    }
                    digits += 1;
                    self.bump();
                }
                Some(d) => {
                    let start = self.pos;
                    self.bump();
                    self.emit_error(QueryLexError::UnicodeEscapeInvalidDigit {
                        span: self.span_from(start),
                        ch: d,
                    });
                    invalid = true;
                }
            }
        }

        if invalid {
            return;
        }
        if digits > 6 {
            self.emit_error(QueryLexError::UnicodeEscapeTooManyDigits {
                span: self.span_from(start),
            });
            return;
        }
        if digits == 0 {
            self.emit_error(QueryLexError::UnicodeEscapeEmpty {
                span: self.span_from(start),
            });
            return;
        }
        match char::from_u32(value) {
            Some(c) => content.push(c),
            None => {
                self.emit_error(QueryLexError::UnicodeEscapeInvalidScalarValue {
                    span: self.span_from(start),
                    value,
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tests::{assert_eof, assert_ident, assert_token};

    #[test]
    fn backtick_ident_escaped_keywords() {
        let mut lex = Lexer::new("`prev` `true` `foo` `let`");
        assert_ident!(lex, "prev");
        assert_ident!(lex, "true");
        assert_ident!(lex, "foo");
        assert_ident!(lex, "let");
        assert_eof!(lex);
    }

    #[test]
    fn unclosed_backtick() {
        let mut lex = Lexer::new("`prev");
        assert_ident!(lex, "prev");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn string_literal() {
        let mut lex = Lexer::new(r#""hello world""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("hello world".into()));
        assert_eof!(lex);
    }

    #[test]
    fn string_escapes() {
        let mut lex = Lexer::new(r#""a\n\t\r\\\"\0""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("a\n\t\r\\\"\0".into()));
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn string_hex_unicode_escapes() {
        let mut lex = Lexer::new(r#""\x41\u{1F600}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("A\u{1F600}".into()));
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn unclosed_string() {
        let mut lex = Lexer::new(r#""hello"#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("hello".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn string_across_lines_error() {
        let mut lex = Lexer::new("\"abc\ndef\"");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("abc".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn non_ascii_hex_escape_error() {
        let mut lex = Lexer::new(r#""\x80""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn invalid_hex_escape_error() {
        let mut lex = Lexer::new(r#""\xG1""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("G1".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unknown_escape_error() {
        let mut lex = Lexer::new(r#""\z""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_empty() {
        let mut lex = Lexer::new(r#""\u{}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_missing_lbrace() {
        let mut lex = Lexer::new(r#""\u41""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("41".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_invalid_digit() {
        let mut lex = Lexer::new(r#""\u{41x}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_too_many_digits() {
        let mut lex = Lexer::new(r#""\u{1234567}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_out_of_range() {
        let mut lex = Lexer::new(r#""\u{110000}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_surrogate() {
        let mut lex = Lexer::new(r#""\u{D800}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }
}
