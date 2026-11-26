use crate::lex::Lexer;
use crate::lex::error::QueryLexError;

impl<'src> Lexer<'src> {
    pub(super) fn skip_whitespace_and_comments(&mut self) {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tests::{assert_eof, assert_ident, assert_token};
    use crate::token::TokenKind;

    #[test]
    fn whitespace_skipped() {
        let mut lex = Lexer::new("  \n\t root  \r\n");
        assert_ident!(lex, "root");
        assert_eof!(lex);
    }

    #[test]
    fn line_comments_skipped() {
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
    fn block_comment_skipped() {
        let mut lex = Lexer::new("/* hi */ users");
        assert_ident!(lex, "users");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn block_comment_multiline() {
        let mut lex = Lexer::new("/* line 1\n line 2 */ true");
        assert_token!(lex, TokenKind::True);
        assert_eof!(lex);
    }

    #[test]
    fn nested_block_comments() {
        let mut lex = Lexer::new("/* a /* b */ c */ users");
        assert_ident!(lex, "users");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
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
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn slash_as_division() {
        let mut lex = Lexer::new("a / b");
        assert_ident!(lex, "a");
        assert_token!(lex, TokenKind::Slash);
        assert_ident!(lex, "b");
        assert_eof!(lex);
    }
}
