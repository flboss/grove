use crate::lex::error::QueryLexError;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Span, Spanned};

mod bytes;
mod comment;
mod duration;
mod error;
mod ident_punct;
mod instant;
mod number;
mod string;

pub struct Lexer<'src> {
    source: &'src str,
    pos: usize,
    peeked: Option<Token>,
    peeked2: Option<Token>,
    diagnostics: Vec<Diagnostic>,
    after_dot: bool,
}

impl<'src> Lexer<'src> {
    pub fn new(source: &'src str) -> Self {
        Lexer {
            source,
            pos: 0,
            peeked: None,
            peeked2: None,
            diagnostics: Vec::new(),
            after_dot: false,
        }
    }

    pub fn next_token(&mut self) -> Token {
        let tok = if let Some(tok) = self.peeked.take() {
            self.peeked = self.peeked2.take();
            tok
        } else {
            self.advance_inner()
        };
        self.after_dot = matches!(tok.value, TokenKind::Dot | TokenKind::QuestionDot(_));
        tok
    }

    pub(crate) fn peek(&mut self) -> &Token {
        if self.peeked.is_none() {
            self.peeked = Some(self.advance());
        }
        self.peeked.as_ref().unwrap()
    }

    #[cfg(test)]
    pub(crate) fn peek2(&mut self) -> &Token {
        self.peek();
        if self.peeked2.is_none() {
            self.peeked2 = Some(self.advance());
        }
        self.peeked2.as_ref().unwrap()
    }

    pub fn take_diagnostics(&mut self) -> Vec<Diagnostic> {
        std::mem::take(&mut self.diagnostics)
    }

    fn advance(&mut self) -> Token {
        let tok = self.advance_inner();
        self.after_dot = matches!(tok.value, TokenKind::Dot | TokenKind::QuestionDot(_));
        tok
    }

    fn advance_inner(&mut self) -> Token {
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
                ';' => self.make_token(start, TokenKind::Semicolon),
                '.' => self.scan_dot(start),
                '$' => {
                    let mut count = 1;
                    while self.peek_char() == Some('$') {
                        self.bump();
                        count += 1;
                    }
                    self.make_token(start, TokenKind::Dollar(count))
                }
                '+' => self.make_token(start, TokenKind::Plus),
                '-' => self.make_token(start, TokenKind::Minus),
                '*' => self.make_token(start, TokenKind::Star),
                '%' => self.make_token(start, TokenKind::Percent),
                '/' => self.make_token(start, TokenKind::Slash),
                '=' => self.scan_pair(start, '=', TokenKind::EqualsEquals, TokenKind::Equals),
                '!' => self.scan_pair(start, '=', TokenKind::BangEquals, TokenKind::Bang),
                '<' => self.scan_pair(start, '=', TokenKind::LessEquals, TokenKind::Less),
                '>' => self.scan_pair(start, '=', TokenKind::GreaterEquals, TokenKind::Greater),
                '&' => self.scan_pair(start, '&', TokenKind::AmpAmp, TokenKind::Ampersand),
                '|' => self.scan_pipe_or_error(start),
                '?' => self.scan_question(start),
                ':' => self.scan_colon(start),
                '"' => self.scan_string(start),
                '`' => self.scan_backtick_ident(start),
                '0'..='9' => {
                    if self.after_dot {
                        return self.scan_postdot_int(start);
                    }
                    return self.scan_number(start);
                }
                '@' => return self.scan_instant(start),
                '#' => return self.scan_duration(start),
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
        Span::new(start, self.pos)
    }

    fn span_current(&self) -> Span {
        let len = self.peek_char().map(char::len_utf8).unwrap_or(0);
        Span::new(self.pos, self.pos + len)
    }

    fn make_eof(&self) -> Token {
        Spanned::new(Span::new(self.pos, self.pos), TokenKind::Eof)
    }

    fn make_token(&self, start: usize, kind: TokenKind) -> Token {
        Spanned::new(Span::new(start, self.pos), kind)
    }

    fn emit_error(&mut self, err: QueryLexError) {
        self.diagnostics.push(err.into());
    }

    fn emit_warning(&mut self, err: QueryLexError) {
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

    pub(super) use assert_eof;
    pub(super) use assert_ident;
    pub(super) use assert_token;

    pub(super) fn drain(lex: &mut Lexer) {
        while !matches!(lex.next_token().value, TokenKind::Eof) {}
    }

    #[test]
    fn empty_input() {
        let mut lex = Lexer::new("");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn invalid_char_error() {
        let mut lex = Lexer::new("a \0 b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn span_offsets() {
        let mut lex = Lexer::new("  foo  ");
        let tok = lex.next_token();
        assert_eq!(tok.span.start, 2);
        assert_eq!(tok.span.end, 5);
        assert!(matches!(tok.value, TokenKind::Ident(_)));
    }
}
