use crate::lex::Lexer;
use crate::lex::error::QueryLexError;
use crate::token::{Token, TokenKind};
use grove_types::Span;

impl<'src> Lexer<'src> {
    pub(super) fn scan_bytes_literal(&mut self, start: usize, radix: BytesRadix) -> Token {
        debug_assert_eq!(self.peek_char(), Some('"'));
        self.bump();

        let content_start = self.pos;
        let (content_end, closed) = loop {
            match self.peek_char() {
                Some('"') => {
                    let end = self.pos;
                    self.bump();
                    break (end, true);
                }
                Some(ch)
                    if ch.is_ascii_alphanumeric()
                        || matches!(ch, '+' | '/' | '_' | ' ' | '\t' | '=') =>
                {
                    self.bump()
                }
                None | Some(_) => {
                    self.emit_error(QueryLexError::UnclosedBytesLiteral {
                        span: self.span_from(start),
                    });
                    break (self.pos, false);
                }
            }
        };

        if !closed {
            return self.make_token(start, TokenKind::BytesLit(Vec::new()));
        }

        let content = &self.source[content_start..content_end];
        let decoded = match radix {
            BytesRadix::Hex => decode_hex_literal(content),
            BytesRadix::Base64 => decode_b64_literal(content),
        };

        let bytes = match decoded {
            Ok(bytes) => bytes,
            Err(err) => {
                self.emit_bytes_decode_error(content_start, content_end, err);
                Vec::new()
            }
        };

        self.make_token(start, TokenKind::BytesLit(bytes))
    }

    fn emit_bytes_decode_error(
        &mut self,
        content_start: usize,
        content_end: usize,
        err: BytesDecodeError,
    ) {
        let char_span = |offset: usize| {
            let start = content_start + offset;
            let end = self.source[start..]
                .chars()
                .next()
                .map_or(start, |ch| start + ch.len_utf8());
            Span::new(start, end)
        };

        let error = match err {
            BytesDecodeError::InvalidHexDigit { offset } => {
                let ch = self.source[content_start + offset..]
                    .chars()
                    .next()
                    .expect("invalid hex digit offset should point at a character");
                QueryLexError::BytesHexInvalidDigit {
                    span: char_span(offset),
                    ch,
                }
            }
            BytesDecodeError::OddHexDigits => QueryLexError::BytesHexOddDigitCount {
                span: Span::new(content_start, content_end),
            },
            BytesDecodeError::InvalidBase64Char { offset } => {
                let ch = self.source[content_start + offset..]
                    .chars()
                    .next()
                    .expect("invalid base64 character offset should point at a character");
                QueryLexError::BytesBase64InvalidChar {
                    span: char_span(offset),
                    ch,
                }
            }
            BytesDecodeError::InvalidBase64Padding { offset } => {
                QueryLexError::BytesBase64InvalidPadding {
                    span: Span::new(content_start + offset, content_end),
                }
            }
            BytesDecodeError::NonZeroTrailingBits { offset } => {
                QueryLexError::BytesBase64NonZeroTrailingBits {
                    span: char_span(offset),
                }
            }
        };

        self.emit_error(error);
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) enum BytesRadix {
    Hex,
    Base64,
}

#[derive(Debug, Clone, Copy)]
enum BytesDecodeError {
    InvalidHexDigit { offset: usize },
    OddHexDigits,
    InvalidBase64Char { offset: usize },
    InvalidBase64Padding { offset: usize },
    NonZeroTrailingBits { offset: usize },
}

fn decode_hex_literal(content: &str) -> Result<Vec<u8>, BytesDecodeError> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut incomplete_byte = false;
    for (offset, ch) in content.char_indices() {
        let digit = match ch {
            ' ' | '\t' | '_' => continue,
            '0'..='9' => ch as u8 - b'0',
            'a'..='f' => ch as u8 - b'a' + 10,
            'A'..='F' => ch as u8 - b'A' + 10,
            _ => return Err(BytesDecodeError::InvalidHexDigit { offset }),
        };
        if incomplete_byte {
            *bytes.last_mut().expect("incomplete byte") |= digit;
        } else {
            bytes.push(digit << 4);
        }
        incomplete_byte = !incomplete_byte;
    }

    if incomplete_byte {
        return Err(BytesDecodeError::OddHexDigits);
    }
    Ok(bytes)
}

fn decode_b64_literal(content: &str) -> Result<Vec<u8>, BytesDecodeError> {
    let mut bytes: Vec<u8> = Vec::new();
    let mut incomplete_chunk = 0u8;
    let mut padding: Vec<usize> = Vec::new();
    for (offset, ch) in content.char_indices() {
        match ch {
            ' ' | '\t' | '_' => continue,
            '=' => padding.push(offset),
            _ => {
                let Some(value) = base64_value(ch) else {
                    return Err(BytesDecodeError::InvalidBase64Char { offset });
                };
                if !padding.is_empty() {
                    return Err(BytesDecodeError::InvalidBase64Padding { offset });
                }
                match incomplete_chunk {
                    0 => bytes.push(value << 2),
                    1 => {
                        *bytes.last_mut().expect("incomplete byte") |= value >> 4;
                        bytes.push(value << 4);
                    }
                    2 => {
                        *bytes.last_mut().expect("incomplete byte") |= value >> 2;
                        bytes.push(value << 6);
                    }
                    _ => {
                        *bytes.last_mut().expect("incomplete byte") |= value;
                        incomplete_chunk = 0;
                        continue;
                    }
                }
                incomplete_chunk += 1;
            }
        }
    }

    match (incomplete_chunk, padding.len()) {
        (0, 0) => {}
        (2, 2) => {
            if bytes.pop().expect("incomplete byte") != 0 {
                return Err(BytesDecodeError::NonZeroTrailingBits {
                    offset: padding.first().unwrap() - 1,
                });
            }
        }
        (3, 1) => {
            if bytes.pop().expect("incomplete byte") != 0 {
                return Err(BytesDecodeError::NonZeroTrailingBits {
                    offset: padding.first().unwrap() - 1,
                });
            }
        }
        _ => {
            return Err(BytesDecodeError::InvalidBase64Padding {
                offset: padding.first().copied().unwrap_or(content.len()),
            });
        }
    }
    Ok(bytes)
}

fn base64_value(ch: char) -> Option<u8> {
    match ch {
        'A'..='Z' => Some(ch as u8 - b'A'),
        'a'..='z' => Some(ch as u8 - b'a' + 26),
        '0'..='9' => Some(ch as u8 - b'0' + 52),
        '+' => Some(62),
        '/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tests::{assert_eof, assert_ident, assert_token};

    #[test]
    fn hex_bytes_literals() {
        for (src, expected) in [
            (
                r#"hex"0E2E 4A68 795B 3D1F""#,
                vec![0x0e, 0x2e, 0x4a, 0x68, 0x79, 0x5b, 0x3d, 0x1f],
            ),
            (r#"hex"DE AD_BE EF""#, vec![0xde, 0xad, 0xbe, 0xef]),
            (r#"hex"""#, Vec::new()),
        ] {
            let mut lex = Lexer::new(src);
            assert_eq!(
                lex.next_token().value,
                TokenKind::BytesLit(expected),
                "for `{src}`"
            );
            assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
        }
    }

    #[test]
    fn base64_bytes_literals() {
        for (src, expected) in [
            (r#"b64"Zg==""#, b"f".to_vec()),
            (r#"b64"Zm8=""#, b"fo".to_vec()),
            (r#"b64"Zm9v""#, b"foo".to_vec()),
            (r#"b64"3q2+_7w==""#, vec![0xde, 0xad, 0xbe, 0xef]),
            (r#"b64" Z m 8 = ""#, b"fo".to_vec()),
            (r#"b64"IGhlbGxvLA==""#, b" hello,".to_vec()),
            (r#"b64"""#, Vec::new()),
        ] {
            let mut lex = Lexer::new(src);
            assert_eq!(
                lex.next_token().value,
                TokenKind::BytesLit(expected),
                "for `{src}`"
            );
            assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
        }
    }

    #[test]
    fn bytes_prefix_immediate() {
        let mut lex = Lexer::new(r#"hex "41" b64 "Zg==" hexdata b64x"#);
        assert_ident!(lex, "hex");
        assert_eq!(lex.next_token().value, TokenKind::StringLit("41".into()));
        assert_ident!(lex, "b64");
        assert_eq!(lex.next_token().value, TokenKind::StringLit("Zg==".into()));
        assert_ident!(lex, "hexdata");
        assert_ident!(lex, "b64x");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn bytes_literal_errors() {
        for (src, expected) in [
            (r#"hex"0g""#, "QL0041"),
            (r#"hex"abc""#, "QL0042"),
            (r#"hex"de"#, "QL0040"),
            (r#"b64"Z-""#, "QL0040"),
            (r#"b64"Zg""#, "QL0044"),
            (r#"b64"Zg===""#, "QL0044"),
            (r#"b64"Z=g=""#, "QL0044"),
            (r#"b64"Zh==""#, "QL0045"),
            (r#"b64"Zm9=""#, "QL0045"),
            (r#"b64"Zg=="#, "QL0040"),
        ] {
            let mut lex = Lexer::new(src);
            assert_eq!(lex.next_token().value, TokenKind::BytesLit(Vec::new()));
            let diags = lex.take_diagnostics();
            let codes: Vec<&str> = diags.iter().map(|d| d.code.as_ref()).collect();
            assert_eq!(codes, vec![expected], "for `{src}`");
        }
    }

    #[test]
    fn bytes_literal_single_line() {
        let mut lex = Lexer::new("hex\"41\n\"");
        assert_eq!(lex.next_token().value, TokenKind::BytesLit(Vec::new()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }
}
