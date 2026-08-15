use crate::error::QueryLexError;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Span, Spanned};

pub struct Lexer<'src> {
    source: &'src str,
    pos: usize,
    peeked: Option<Token>,
    peeked2: Option<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Lexer<'src> {
    pub fn new(source: &'src str) -> Self {
        Lexer {
            source,
            pos: 0,
            peeked: None,
            peeked2: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn next_token(&mut self) -> Token {
        if let Some(tok) = self.peeked.take() {
            self.peeked = self.peeked2.take();
            return tok;
        }
        self.advance()
    }

    pub fn peek(&mut self) -> &Token {
        if self.peeked.is_none() {
            self.peeked = Some(self.advance());
        }
        self.peeked.as_ref().unwrap()
    }

    pub fn peek2(&mut self) -> &Token {
        self.peek();
        if self.peeked2.is_none() {
            self.peeked2 = Some(self.advance());
        }
        self.peeked2.as_ref().unwrap()
    }

    pub fn finalize(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    fn advance(&mut self) -> Token {
        loop {
            self.skip_whitespace_and_comments();
            let start = self.pos;

            let Some(ch) = self.peek_char() else {
                return self.make_eof();
            };
            self.bump();

            if ch.is_ascii_alphabetic() || ch == '_' {
                return self.scan_ident_or_keyword(start);
            }

            let token = match ch {
                '{' => self.make_token(start, TokenKind::LBrace),
                '}' => self.make_token(start, TokenKind::RBrace),
                '[' => self.make_token(start, TokenKind::LBracket),
                ']' => self.make_token(start, TokenKind::RBracket),
                '(' => self.make_token(start, TokenKind::LParen),
                ')' => self.make_token(start, TokenKind::RParen),
                ',' => self.make_token(start, TokenKind::Comma),
                '.' => self.make_token(start, TokenKind::Dot),
                '+' => self.make_token(start, TokenKind::Plus),
                '-' => self.make_token(start, TokenKind::Minus),
                '*' => self.make_token(start, TokenKind::Star),
                '%' => self.make_token(start, TokenKind::Percent),
                '/' => self.make_token(start, TokenKind::Slash),
                '=' => self.scan_pair(start, '=', TokenKind::EqualsEquals, TokenKind::Equals),
                '!' => self.scan_pair(start, '=', TokenKind::BangEquals, TokenKind::Bang),
                '<' => self.scan_pair(start, '=', TokenKind::LessEquals, TokenKind::Less),
                '>' => self.scan_pair(start, '=', TokenKind::GreaterEquals, TokenKind::Greater),
                '&' => self.scan_amp_or_error(start),
                '|' => self.scan_pipe_or_error(start),
                '?' => self.scan_question_or_error(start),
                ':' => self.scan_colon_or_error(start),
                '"' => self.scan_string(start),
                '`' => self.scan_backtick_ident(start),
                '0'..='9' | '@' | '#' => {
                    // TODO: Int/Float/Dec/Instant/Duration literals
                    continue;
                }
                _ => {
                    self.emit_error(QueryLexError::UnexpectedChar {
                        span: self.span_from(start),
                        ch,
                    });
                    continue;
                }
            };

            return token;
        }
    }

    fn scan_ident_or_keyword(&mut self, start: usize) -> Token {
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                self.bump();
            } else {
                break;
            }
        }

        let word = &self.source[start..self.pos];
        let kind = match word {
            "none" => TokenKind::None,
            "some" => TokenKind::Some,
            "true" => TokenKind::True,
            "false" => TokenKind::False,
            "prev" => TokenKind::Prev,
            "in" => TokenKind::In,
            "as" => TokenKind::As,
            "if" => TokenKind::If,
            "else" => TokenKind::Else,
            "mod" => TokenKind::Mod,
            "asc" => TokenKind::Asc,
            "desc" => TokenKind::Desc,
            "Int" => TokenKind::Int,
            "Float" => TokenKind::Float,
            "Dec" => TokenKind::Dec,
            "Bool" => TokenKind::Bool,
            "String" => TokenKind::String,
            _ => TokenKind::Ident(word.to_string()),
        };

        self.make_token(start, kind)
    }

    fn scan_pair(
        &mut self,
        start: usize,
        second: char,
        both: TokenKind,
        single: TokenKind,
    ) -> Token {
        if self.peek_char() == Some(second) {
            self.bump();
            self.make_token(start, both)
        } else {
            self.make_token(start, single)
        }
    }

    fn scan_amp_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('&') {
            self.bump();
            return self.make_token(start, TokenKind::AmpAmp);
        }
        self.emit_error(QueryLexError::UnexpectedAmpersand {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_pipe_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('|') {
            self.bump();
            return self.make_token(start, TokenKind::PipePipe);
        }
        self.emit_error(QueryLexError::UnexpectedPipe {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_question_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('.') {
            self.bump();
            return self.make_token(start, TokenKind::QuestionDot);
        }
        self.emit_error(QueryLexError::UnexpectedQuestion {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_colon_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some(':') {
            self.bump();
            return self.make_token(start, TokenKind::DoubleColon);
        }
        self.emit_error(QueryLexError::UnexpectedColon {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_backtick_ident(&mut self, start: usize) -> Token {
        let mut content = String::new();
        loop {
            match self.peek_char() {
                None | Some('\n') => {
                    self.emit_error(QueryLexError::UnclosedBacktickIdent {
                        span: self.span_from(start),
                    });
                    break;
                }
                Some('`') => {
                    self.bump();
                    break;
                }
                Some(ch) => {
                    self.bump();
                    content.push(ch);
                }
            }
        }
        self.make_token(start, TokenKind::Ident(content))
    }

    fn scan_string(&mut self, start: usize) -> Token {
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

    fn skip_whitespace_and_comments(&mut self) {
        loop {
            match (self.peek_char(), self.peek_char2()) {
                (Some(ch), _) if ch.is_whitespace() => {
                    self.bump();
                }
                (Some('/'), Some('/')) => {
                    self.bump();
                    self.bump();
                    while let Some(ch) = self.peek_char() {
                        if ch == '\n' {
                            break;
                        }
                        self.bump();
                    }
                }
                (Some('/'), Some('*')) => {
                    self.scan_block_comment();
                }
                _ => break,
            }
        }
    }

    fn scan_block_comment(&mut self) {
        let start = self.pos;
        self.bump();
        self.bump();
        let mut depth = 1u32;
        loop {
            match (self.peek_char(), self.peek_char2()) {
                (Some('/'), Some('*')) => {
                    depth += 1;
                    self.bump();
                    self.bump();
                }
                (Some('*'), Some('/')) => {
                    depth -= 1;
                    self.bump();
                    self.bump();
                    if depth == 0 {
                        return;
                    }
                }
                (Some(_), _) => {
                    self.bump();
                }
                (None, _) => {
                    self.emit_error(QueryLexError::UnclosedBlockComment {
                        span: self.span_from(start),
                    });
                    return;
                }
            }
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.source[self.pos..].chars().next()
    }

    fn peek_char2(&self) -> Option<char> {
        self.source[self.pos..].chars().nth(1)
    }

    fn bump(&mut self) {
        if let Some(ch) = self.peek_char() {
            self.pos += ch.len_utf8();
        }
    }

    fn span_from(&self, start: usize) -> Span {
        Span {
            start,
            end: self.pos,
        }
    }

    fn make_eof(&self) -> Token {
        Spanned {
            span: Span {
                start: self.pos,
                end: self.pos,
            },
            value: TokenKind::Eof,
        }
    }

    fn make_token(&self, start: usize, kind: TokenKind) -> Token {
        Spanned {
            span: Span {
                start,
                end: self.pos,
            },
            value: kind,
        }
    }

    fn emit_error(&mut self, err: QueryLexError) {
        self.diagnostics.push(err.into());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! assert_token {
        ($lexer:expr, $kind:pat) => {
            let tok = $lexer.next_token();
            assert!(
                matches!(tok.value, $kind),
                "expected token kind {}, got {:?}",
                stringify!($kind),
                tok.value,
            );
        };
    }

    macro_rules! assert_ident {
        ($lexer:expr, $name:expr) => {
            let tok = $lexer.next_token();
            match tok.value {
                TokenKind::Ident(s) => assert_eq!(s, $name),
                ref other => panic!("expected Ident({}), got {:?}", $name, other),
            }
        };
    }

    macro_rules! assert_eof {
        ($lexer:expr) => {
            assert_token!($lexer, TokenKind::Eof);
        };
    }

    #[test]
    fn empty_input() {
        let mut lex = Lexer::new("");
        assert_eof!(lex);
        assert!(lex.finalize().is_empty());
    }

    #[test]
    fn whitespace_is_skipped() {
        let mut lex = Lexer::new("  \n\t root  \r\n");
        assert_ident!(lex, "root");
        assert_eof!(lex);
    }

    #[test]
    fn line_comments_are_skipped() {
        let mut lex = Lexer::new("// a comment\nusers");
        assert_ident!(lex, "users");
        assert_eof!(lex);
    }

    #[test]
    fn line_comment_at_eof() {
        let mut lex = Lexer::new("// just a comment");
        assert_eof!(lex);
    }

    #[test]
    fn line_comment_between_tokens() {
        let mut lex = Lexer::new("a // note\n b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eof!(lex);
    }

    #[test]
    fn block_comment_is_skipped() {
        let mut lex = Lexer::new("/* hi */ users");
        assert_ident!(lex, "users");
        assert_eof!(lex);
        assert!(lex.finalize().is_empty());
    }

    #[test]
    fn block_comment_spans_lines() {
        let mut lex = Lexer::new("/* line 1\n line 2 */ true");
        assert_token!(lex, TokenKind::True);
        assert_eof!(lex);
    }

    #[test]
    fn nested_block_comments() {
        let mut lex = Lexer::new("/* a /* b */ c */ users");
        assert_ident!(lex, "users");
        assert_eof!(lex);
        assert!(lex.finalize().is_empty());
    }

    #[test]
    fn block_comment_inside_line_comment() {
        let mut lex = Lexer::new("// /* not a comment\nusers");
        assert_ident!(lex, "users");
        assert_eof!(lex);
    }

    #[test]
    fn unclosed_block_comment() {
        let mut lex = Lexer::new("/* never closed");
        assert_eof!(lex);
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn slash_as_division() {
        let mut lex = Lexer::new("a / b");
        assert_ident!(lex, "a");
        assert_token!(lex, TokenKind::Slash);
        assert_ident!(lex, "b");
        assert_eof!(lex);
    }

    #[test]
    fn keywords() {
        let mut lex = Lexer::new("none some true false prev in as if else mod asc desc");
        assert_token!(lex, TokenKind::None);
        assert_token!(lex, TokenKind::Some);
        assert_token!(lex, TokenKind::True);
        assert_token!(lex, TokenKind::False);
        assert_token!(lex, TokenKind::Prev);
        assert_token!(lex, TokenKind::In);
        assert_token!(lex, TokenKind::As);
        assert_token!(lex, TokenKind::If);
        assert_token!(lex, TokenKind::Else);
        assert_token!(lex, TokenKind::Mod);
        assert_token!(lex, TokenKind::Asc);
        assert_token!(lex, TokenKind::Desc);
        assert_eof!(lex);
    }

    #[test]
    fn type_keywords() {
        let mut lex = Lexer::new("Int Float Dec Bool String");
        assert_token!(lex, TokenKind::Int);
        assert_token!(lex, TokenKind::Float);
        assert_token!(lex, TokenKind::Dec);
        assert_token!(lex, TokenKind::Bool);
        assert_token!(lex, TokenKind::String);
        assert_eof!(lex);
    }

    #[test]
    fn idents() {
        let mut lex = Lexer::new("foo _bar baz123");
        assert_ident!(lex, "foo");
        assert_ident!(lex, "_bar");
        assert_ident!(lex, "baz123");
        assert_eof!(lex);
    }

    #[test]
    fn ident_does_not_match_keyword() {
        let mut lex = Lexer::new("incoming truex Intx");
        assert_ident!(lex, "incoming");
        assert_ident!(lex, "truex");
        assert_ident!(lex, "Intx");
        assert_eof!(lex);
    }

    #[test]
    fn backtick_ident_escapes_keywords() {
        let mut lex = Lexer::new("`prev` `true` `foo`");
        assert_ident!(lex, "prev");
        assert_ident!(lex, "true");
        assert_ident!(lex, "foo");
        assert_eof!(lex);
    }

    #[test]
    fn unclosed_backtick() {
        let mut lex = Lexer::new("`prev");
        assert_ident!(lex, "prev");
        assert_eq!(lex.finalize().len(), 1);
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
        assert!(lex.finalize().is_empty());
    }

    #[test]
    fn string_hex_and_unicode_escapes() {
        let mut lex = Lexer::new(r#""\x41\u{1F600}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("A\u{1F600}".into()));
        assert!(lex.finalize().is_empty());
    }

    #[test]
    fn unclosed_string() {
        let mut lex = Lexer::new(r#""hello"#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("hello".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn string_cannot_span_lines() {
        let mut lex = Lexer::new("\"abc\ndef\"");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("abc".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn non_ascii_hex_escape_is_error() {
        let mut lex = Lexer::new(r#""\x80""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn invalid_hex_escape_is_error() {
        let mut lex = Lexer::new(r#""\xG1""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("G1".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn unknown_escape_is_error() {
        let mut lex = Lexer::new(r#""\z""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn unicode_escape_empty() {
        let mut lex = Lexer::new(r#""\u{}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn unicode_escape_missing_lbrace() {
        let mut lex = Lexer::new(r#""\u41""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("41".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn unicode_escape_invalid_digit() {
        let mut lex = Lexer::new(r#""\u{41x}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn unicode_escape_too_many_digits() {
        let mut lex = Lexer::new(r#""\u{1234567}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn unicode_escape_out_of_range() {
        let mut lex = Lexer::new(r#""\u{110000}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn unicode_escape_surrogate() {
        let mut lex = Lexer::new(r#""\u{D800}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn punctuation() {
        let mut lex = Lexer::new("{}[](),.+*%-");
        assert_token!(lex, TokenKind::LBrace);
        assert_token!(lex, TokenKind::RBrace);
        assert_token!(lex, TokenKind::LBracket);
        assert_token!(lex, TokenKind::RBracket);
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        assert_token!(lex, TokenKind::Comma);
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::Plus);
        assert_token!(lex, TokenKind::Star);
        assert_token!(lex, TokenKind::Percent);
        assert_token!(lex, TokenKind::Minus);
        assert_eof!(lex);
    }

    #[test]
    fn two_char_operators() {
        let mut lex = Lexer::new("== != <= >= && || :: ?.");
        assert_token!(lex, TokenKind::EqualsEquals);
        assert_token!(lex, TokenKind::BangEquals);
        assert_token!(lex, TokenKind::LessEquals);
        assert_token!(lex, TokenKind::GreaterEquals);
        assert_token!(lex, TokenKind::AmpAmp);
        assert_token!(lex, TokenKind::PipePipe);
        assert_token!(lex, TokenKind::DoubleColon);
        assert_token!(lex, TokenKind::QuestionDot);
        assert_eof!(lex);
    }

    #[test]
    fn single_char_operator_forms() {
        let mut lex = Lexer::new("= ! < >");
        assert_token!(lex, TokenKind::Equals);
        assert_token!(lex, TokenKind::Bang);
        assert_token!(lex, TokenKind::Less);
        assert_token!(lex, TokenKind::Greater);
        assert_eof!(lex);
    }

    #[test]
    fn lone_ampersand_is_error() {
        let mut lex = Lexer::new("a & b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn lone_pipe_is_error() {
        let mut lex = Lexer::new("a | b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn lone_question_is_error() {
        let mut lex = Lexer::new("a ? b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn lone_colon_is_error() {
        let mut lex = Lexer::new("a : b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn invalid_char_is_error() {
        let mut lex = Lexer::new("a \0 b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn peek_peek2_token_lookahead() {
        let mut lex = Lexer::new("users true false");
        assert_eq!(lex.peek().value, TokenKind::Ident("users".into()));
        assert_eq!(lex.peek2().value, TokenKind::True);
        assert_eq!(lex.peek().value, TokenKind::Ident("users".into()));
        assert_eq!(lex.next_token().value, TokenKind::Ident("users".into()));
        assert_eq!(lex.peek().value, TokenKind::True);
        assert_eq!(lex.peek2().value, TokenKind::False);
        assert_eq!(lex.next_token().value, TokenKind::True);
        assert_eq!(lex.peek2().value, TokenKind::Eof);
    }

    #[test]
    fn span_offsets() {
        let mut lex = Lexer::new("  foo  ");
        let tok = lex.next_token();
        assert_eq!(tok.span.start, 2);
        assert_eq!(tok.span.end, 5);
        assert!(matches!(tok.value, TokenKind::Ident(_)));
    }

    #[test]
    fn span_of_keyword() {
        let mut lex = Lexer::new("in");
        let tok = lex.next_token();
        assert_eq!(tok.span.start, 0);
        assert_eq!(tok.span.end, 2);
        assert_eq!(tok.value, TokenKind::In);
    }
}
