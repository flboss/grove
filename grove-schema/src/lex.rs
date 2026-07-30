use crate::error::SchemaParseError;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Span, Spanned};

pub struct Lexer<'src> {
    source: &'src str,
    pos: usize,
    peeked: Option<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Lexer<'src> {
    pub fn new(source: &'src str) -> Self {
        Lexer {
            source,
            pos: 0,
            peeked: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn next_token(&mut self) -> Token {
        if let Some(tok) = self.peeked.take() {
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

    pub fn finalize(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    fn advance(&mut self) -> Token {
        loop {
            self.skip_whitespace_and_comments();
            let start = self.pos;

            let Some(ch) = self.source[start..].chars().next() else {
                return self.make_eof();
            };
            self.pos += ch.len_utf8();

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
                ':' => self.make_token(start, TokenKind::Colon),
                ';' => self.make_token(start, TokenKind::Semicolon),
                '=' => self.make_token(start, TokenKind::Equals),
                ',' => self.make_token(start, TokenKind::Comma),
                '.' => self.make_token(start, TokenKind::Dot),
                '@' => self.make_token(start, TokenKind::At),
                '?' => self.make_token(start, TokenKind::Question),
                '>' => self.make_token(start, TokenKind::RAngle),
                '<' => self.scan_arrow_or_langle(start),
                '-' => self.scan_arrow_or_minus(start),
                '"' => self.scan_string(start),
                '`' => self.scan_backtick_ident(start),
                _ => {
                    self.emit_error(SchemaParseError::UnexpectedChar {
                        span: Span {
                            start,
                            end: self.pos,
                        },
                        ch,
                    });
                    continue; // try again from next char
                }
            };

            return token;
        }
    }

    fn scan_ident_or_keyword(&mut self, start: usize) -> Token {
        while let Some(ch) = self.source[self.pos..].chars().next() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                self.pos += ch.len_utf8();
            } else {
                break;
            }
        }

        let word = &self.source[start..self.pos];
        let kind = match word {
            "config" => TokenKind::Config,
            "root" => TokenKind::Root,
            "struct" => TokenKind::Struct,
            "rel" => TokenKind::Rel,
            "via" => TokenKind::Via,
            "List" => TokenKind::List,
            "Tuple" => TokenKind::Tuple,
            "Int" => TokenKind::Int,
            "Float" => TokenKind::Float,
            "Dec" => TokenKind::Dec,
            "String" => TokenKind::String,
            "Bool" => TokenKind::Bool,
            "Instant" => TokenKind::Instant,
            "Duration" => TokenKind::Duration,
            _ => TokenKind::Ident(word.to_string()),
        };

        self.make_token(start, kind)
    }

    fn scan_arrow_or_langle(&mut self, start: usize) -> Token {
        for (pattern, kind) in [
            ("<->>", TokenKind::ManyToMany),
            ("<->", TokenKind::ManyToOne),
            ("->>", TokenKind::OneToMany),
            ("->", TokenKind::OneToOne),
        ] {
            if self.source[self.pos..].starts_with(pattern) {
                self.pos += pattern.len();
                return self.make_token(start, kind);
            }
        }

        self.make_token(start, TokenKind::LAngle)
    }

    fn scan_arrow_or_minus(&mut self, start: usize) -> Token {
        if self.source[self.pos..].starts_with('>') {
            self.pos += '>'.len_utf8();
            return self.make_token(start, TokenKind::Arrow);
        }

        // Lone '-' is an error
        self.emit_error(SchemaParseError::UnexpectedChar {
            span: Span {
                start,
                end: self.pos,
            },
            ch: '-',
        });
        self.advance()
    }

    fn scan_string(&mut self, start: usize) -> Token {
        let content_start = self.pos;

        for ch in self.source[self.pos..].chars() {
            match ch {
                '\n' => break,
                '"' => {
                    let content = self.source[content_start..self.pos].to_string();
                    self.pos += '"'.len_utf8();
                    return self.make_token(start, TokenKind::StringLit(content));
                }
                ch => {
                    self.pos += ch.len_utf8();
                }
            }
        }

        self.emit_error(SchemaParseError::UnclosedString {
            span: Span {
                start,
                end: self.pos,
            },
        });
        let content = self.source[content_start..self.pos].to_string();
        self.make_token(start, TokenKind::StringLit(content))
    }

    fn scan_backtick_ident(&mut self, start: usize) -> Token {
        let content_start = self.pos;

        for ch in self.source[self.pos..].chars() {
            match ch {
                '\n' => break,
                '`' => {
                    let content = self.source[content_start..self.pos].to_string();
                    self.pos += '`'.len_utf8();
                    return self.make_token(start, TokenKind::Ident(content));
                }
                ch => {
                    self.pos += ch.len_utf8();
                }
            }
        }

        self.emit_error(SchemaParseError::UnclosedBacktickIdent {
            span: Span {
                start,
                end: self.pos,
            },
        });
        let content = self.source[content_start..self.pos].to_string();
        self.make_token(start, TokenKind::Ident(content))
    }

    fn skip_whitespace_and_comments(&mut self) {
        let mut chars = self.source[self.pos..].chars();
        loop {
            match chars.next() {
                Some(ch) if ch.is_whitespace() => {
                    self.pos += ch.len_utf8();
                }
                Some('#') => {
                    self.pos += '#'.len_utf8();
                    for ch in chars.by_ref() {
                        if ch == '\n' {
                            self.pos += ch.len_utf8();
                            break;
                        }
                        self.pos += ch.len_utf8();
                    }
                }
                _ => break,
            }
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

    fn emit_error(&mut self, err: SchemaParseError) {
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
    fn comments_are_skipped() {
        let mut lex = Lexer::new("# this is a comment\nroot");
        assert_token!(lex, TokenKind::Root);
        assert_eof!(lex);
    }

    #[test]
    fn comment_at_eof() {
        let mut lex = Lexer::new("# just a comment");
        assert_eof!(lex);
    }

    #[test]
    fn keywords() {
        let mut lex = Lexer::new("config root struct rel via");
        assert_token!(lex, TokenKind::Config);
        assert_token!(lex, TokenKind::Root);
        assert_token!(lex, TokenKind::Struct);
        assert_token!(lex, TokenKind::Rel);
        assert_token!(lex, TokenKind::Via);
        assert_eof!(lex);
    }

    #[test]
    fn type_keywords() {
        let mut lex = Lexer::new("Int Float Dec String Bool Instant Duration List Tuple");
        assert_token!(lex, TokenKind::Int);
        assert_token!(lex, TokenKind::Float);
        assert_token!(lex, TokenKind::Dec);
        assert_token!(lex, TokenKind::String);
        assert_token!(lex, TokenKind::Bool);
        assert_token!(lex, TokenKind::Instant);
        assert_token!(lex, TokenKind::Duration);
        assert_token!(lex, TokenKind::List);
        assert_token!(lex, TokenKind::Tuple);
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
        let mut lex = Lexer::new("configx");
        assert_ident!(lex, "configx");
    }

    #[test]
    fn backtick_ident_escapes_keywords() {
        let mut lex = Lexer::new("`root` `struct` `foo`");
        assert_ident!(lex, "root");
        assert_ident!(lex, "struct");
        assert_ident!(lex, "foo");
        assert_eof!(lex);
    }

    #[test]
    fn unclosed_backtick() {
        let mut lex = Lexer::new("`root");
        assert_ident!(lex, "root");
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
    fn unclosed_string() {
        let mut lex = Lexer::new(r#""hello"#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("hello".into()));
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn punctuation() {
        let mut lex = Lexer::new("{}[]()<>:;,=.@?");
        assert_token!(lex, TokenKind::LBrace);
        assert_token!(lex, TokenKind::RBrace);
        assert_token!(lex, TokenKind::LBracket);
        assert_token!(lex, TokenKind::RBracket);
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        assert_token!(lex, TokenKind::LAngle);
        assert_token!(lex, TokenKind::RAngle);
        assert_token!(lex, TokenKind::Colon);
        assert_token!(lex, TokenKind::Semicolon);
        assert_token!(lex, TokenKind::Comma);
        assert_token!(lex, TokenKind::Equals);
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::At);
        assert_token!(lex, TokenKind::Question);
        assert_eof!(lex);
    }

    #[test]
    fn arrow() {
        let mut lex = Lexer::new("->");
        assert_token!(lex, TokenKind::Arrow);
        assert_eof!(lex);
    }

    #[test]
    fn type_bracket_after_keyword() {
        let mut lex = Lexer::new("List<Int>");
        assert_token!(lex, TokenKind::List);
        assert_token!(lex, TokenKind::LAngle);
        assert_token!(lex, TokenKind::Int);
        assert_token!(lex, TokenKind::RAngle);
        assert_eof!(lex);
    }

    #[test]
    fn tuple_brackets() {
        let mut lex = Lexer::new("Tuple<Int, String>");
        assert_token!(lex, TokenKind::Tuple);
        assert_token!(lex, TokenKind::LAngle);
        assert_token!(lex, TokenKind::Int);
        assert_token!(lex, TokenKind::Comma);
        assert_token!(lex, TokenKind::String);
        assert_token!(lex, TokenKind::RAngle);
        assert_eof!(lex);
    }

    #[test]
    fn langle_rangle_arrow_disambiguation() {
        let mut lex = Lexer::new("< <<->> > <->");
        assert_token!(lex, TokenKind::LAngle);
        assert_token!(lex, TokenKind::ManyToMany);
        assert_token!(lex, TokenKind::RAngle);
        assert_token!(lex, TokenKind::OneToOne);
        assert_eof!(lex);
    }

    #[test]
    fn relation_arrows() {
        let mut lex = Lexer::new("<<->> <<-> <->> <->");
        assert_token!(lex, TokenKind::ManyToMany);
        assert_token!(lex, TokenKind::ManyToOne);
        assert_token!(lex, TokenKind::OneToMany);
        assert_token!(lex, TokenKind::OneToOne);
        assert_eof!(lex);
    }

    #[test]
    fn lone_less_than_is_langle() {
        let mut lex = Lexer::new("<");
        assert_token!(lex, TokenKind::LAngle);
        assert_eof!(lex);
        assert!(lex.finalize().is_empty());
    }

    #[test]
    fn lone_minus_is_error() {
        let mut lex = Lexer::new("-");
        assert_eof!(lex);
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn error_recovery_skips_bad_chars() {
        let mut lex = Lexer::new("@good @\0@bad");
        assert_token!(lex, TokenKind::At);
        assert_ident!(lex, "good");
        assert_token!(lex, TokenKind::At);
        // \0 is skipped as an error, then @bad continues
        assert_token!(lex, TokenKind::At);
        assert_ident!(lex, "bad");
        assert_eof!(lex);
        assert_eq!(lex.finalize().len(), 1);
    }

    #[test]
    fn peek_does_not_advance() {
        let mut lex = Lexer::new("root struct");
        assert_eq!(lex.peek().value, TokenKind::Root);
        assert_eq!(lex.peek().value, TokenKind::Root);
        assert_eq!(lex.next_token().value, TokenKind::Root);
        assert_eq!(lex.peek().value, TokenKind::Struct);
    }

    #[test]
    fn whitespace_is_skipped() {
        let mut lex = Lexer::new("  \n\t root  \n");
        assert_token!(lex, TokenKind::Root);
        assert_eof!(lex);
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
        let mut lex = Lexer::new("root");
        let tok = lex.next_token();
        assert_eq!(tok.span.start, 0);
        assert_eq!(tok.span.end, 4);
        assert_eq!(tok.value, TokenKind::Root);
    }
}
