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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tests::{assert_eof, assert_ident, assert_token};

    #[test]
    fn keywords() {
        let mut lex = Lexer::new("none some true false prev in as if else mod asc desc let");
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
        assert_token!(lex, TokenKind::Let);
        assert_eof!(lex);
    }

    #[test]
    fn type_keywords() {
        let mut lex = Lexer::new("Int Float Dec String Bytes List Bool Instant Duration");
        assert_token!(lex, TokenKind::Int);
        assert_token!(lex, TokenKind::Float);
        assert_token!(lex, TokenKind::Dec);
        assert_token!(lex, TokenKind::String);
        assert_token!(lex, TokenKind::Bytes);
        assert_token!(lex, TokenKind::List);
        assert_token!(lex, TokenKind::Bool);
        assert_token!(lex, TokenKind::Instant);
        assert_token!(lex, TokenKind::Duration);
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
    fn ident_partial_keyword_not_matched() {
        let mut lex = Lexer::new("incoming truex Intx outlet Listx");
        assert_ident!(lex, "incoming");
        assert_ident!(lex, "truex");
        assert_ident!(lex, "Intx");
        assert_ident!(lex, "outlet");
        assert_ident!(lex, "Listx");
        assert_eof!(lex);
    }

    #[test]
    fn punctuation() {
        let mut lex = Lexer::new("{}[](),;.+*%-");
        assert_token!(lex, TokenKind::LBrace);
        assert_token!(lex, TokenKind::RBrace);
        assert_token!(lex, TokenKind::LBracket);
        assert_token!(lex, TokenKind::RBracket);
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        assert_token!(lex, TokenKind::Comma);
        assert_token!(lex, TokenKind::Semicolon);
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::Plus);
        assert_token!(lex, TokenKind::Star);
        assert_token!(lex, TokenKind::Percent);
        assert_token!(lex, TokenKind::Minus);
        assert_eof!(lex);
    }

    #[test]
    fn triple_dot() {
        let mut lex = Lexer::new(".. ... .0...");
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::TripleDot);
        assert_token!(lex, TokenKind::Dot);
        assert_eq!(lex.next_token().value, TokenKind::IntLit(0));
        assert_token!(lex, TokenKind::TripleDot);
        assert_eof!(lex);
    }

    #[test]
    fn as_question() {
        let mut lex = Lexer::new("as? as ?Int");
        assert_token!(lex, TokenKind::AsQuestion);
        assert_token!(lex, TokenKind::As);
        assert_token!(lex, TokenKind::Question(1));
        assert_token!(lex, TokenKind::Int);
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
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
        assert_token!(lex, TokenKind::QuestionDot(1));
        assert_eof!(lex);
    }

    #[test]
    fn question_dot_depth() {
        let mut lex = Lexer::new("?. ??. ????. foo??.1");
        assert_token!(lex, TokenKind::QuestionDot(1));
        assert_token!(lex, TokenKind::QuestionDot(2));
        assert_token!(lex, TokenKind::QuestionDot(4));
        assert_ident!(lex, "foo");
        assert_token!(lex, TokenKind::QuestionDot(2));
        assert_eq!(lex.next_token().value, TokenKind::IntLit(1));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn question_depth() {
        for (source, depth) in [("?", 1), ("??", 2), ("????", 4)] {
            let mut lex = Lexer::new(source);
            assert_eq!(lex.next_token().value, TokenKind::Question(depth));
            assert_eof!(lex);
            assert!(lex.take_diagnostics().is_empty(), "{source}");
        }
        let mut lex = Lexer::new("??x");
        assert_token!(lex, TokenKind::Question(2));
        assert_ident!(lex, "x");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
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
    fn ampersand_ref_logical_and() {
        let mut lex = Lexer::new("& && &&&");
        assert_token!(lex, TokenKind::Ampersand);
        assert_token!(lex, TokenKind::AmpAmp);
        assert_token!(lex, TokenKind::AmpAmp);
        assert_token!(lex, TokenKind::Ampersand);
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn lone_pipe_error() {
        let mut lex = Lexer::new("a | b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn lone_colon() {
        let mut lex = Lexer::new("a : b");
        assert_ident!(lex, "a");
        assert_token!(lex, TokenKind::Colon);
        assert_ident!(lex, "b");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn postdot_digits_as_int() {
        let mut lex = Lexer::new("trio.2.1");
        assert_ident!(lex, "trio");
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::IntLit(2));
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::IntLit(1));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn dollar_token_count() {
        let mut lex = Lexer::new("$$ $$$ $");
        assert_token!(lex, TokenKind::Dollar(2));
        assert_token!(lex, TokenKind::Dollar(3));
        assert_token!(lex, TokenKind::Dollar(1));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
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
    fn span_of_keyword() {
        let mut lex = Lexer::new("in");
        let tok = lex.next_token();
        assert_eq!(tok.span.start, 0);
        assert_eq!(tok.span.end, 2);
        assert_eq!(tok.value, TokenKind::In);
    }
}
