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
