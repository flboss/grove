use super::bytes::BytesRadix;
use crate::lex::Lexer;
use crate::lex::error::QueryLexError;
use crate::token::{Token, TokenKind};

impl<'src> Lexer<'src> {
    pub(super) fn scan_ident_or_keyword(&mut self, start: usize) -> Token {
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                self.bump();
            } else {
                break;
            }
        }

        let word = &self.source[start..self.pos];
        if word == "as" && self.peek_char() == Some('?') {
            self.bump();
            return self.make_token(start, TokenKind::AsQuestion);
        }
        if self.peek_char() == Some('"') {
            match word {
                "hex" => return self.scan_bytes_literal(start, BytesRadix::Hex),
                "b64" => return self.scan_bytes_literal(start, BytesRadix::Base64),
                _ => {}
            }
        }
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
            "let" => TokenKind::Let,
            "Int" => TokenKind::Int,
            "Float" => TokenKind::Float,
            "Dec" => TokenKind::Dec,
            "String" => TokenKind::String,
            "Bytes" => TokenKind::Bytes,
            "List" => TokenKind::List,
            "Bool" => TokenKind::Bool,
            "Instant" => TokenKind::Instant,
            "Duration" => TokenKind::Duration,
            _ => TokenKind::Ident(word.to_string()),
        };

        self.make_token(start, kind)
    }

    pub(super) fn scan_pair(
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

    pub(super) fn scan_dot(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('.') && self.peek_char2() == Some('.') {
            self.bump();
            self.bump();
            return self.make_token(start, TokenKind::TripleDot);
        }
        self.make_token(start, TokenKind::Dot)
    }

    pub(super) fn scan_pipe_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('|') {
            self.bump();
            return self.make_token(start, TokenKind::PipePipe);
        }
        self.emit_error(QueryLexError::UnexpectedPipe {
            span: self.span_from(start),
        });
        self.advance()
    }

    pub(super) fn scan_question(&mut self, start: usize) -> Token {
        let mut depth = 1;
        while self.peek_char() == Some('?') {
            self.bump();
            depth += 1;
        }
        if self.peek_char() == Some('.') {
            self.bump();
            return self.make_token(start, TokenKind::QuestionDot(depth));
        }
        self.make_token(start, TokenKind::Question(depth))
    }

    pub(super) fn scan_colon(&mut self, start: usize) -> Token {
        if self.peek_char() == Some(':') {
            self.bump();
            return self.make_token(start, TokenKind::DoubleColon);
        }
        self.make_token(start, TokenKind::Colon)
    }

    pub(super) fn scan_postdot_int(&mut self, start: usize) -> Token {
        self.scan_ascii_digits();
        match self.source[start..self.pos].parse::<i64>() {
            Ok(n) => self.make_token(start, TokenKind::IntLit(n)),
            Err(_) => {
                self.emit_error(QueryLexError::IntLiteralOverflow {
                    span: self.span_from(start),
                });
                self.advance()
            }
        }
    }
}
