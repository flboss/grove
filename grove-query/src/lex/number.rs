use rust_decimal::Decimal;

use crate::lex::Lexer;
use crate::lex::error::QueryLexError;
use crate::token::{Token, TokenKind};

impl<'src> Lexer<'src> {
    pub(super) fn scan_number(&mut self, start: usize) -> Token {
        self.scan_ascii_digits();
        let int_digits = self.source[start..self.pos].to_string();

        let mut frac_digits = String::new();
        if self.peek_char() == Some('.')
            && matches!(self.peek_char2(), Some(c) if c.is_ascii_digit())
        {
            self.bump();
            frac_digits = self.scan_ascii_digits();
        }

        let mut exp: i128 = 0;
        let mut has_exp = false;
        if matches!(self.peek_char(), Some('e') | Some('E')) {
            self.bump();
            let mut sign: i128 = 1;
            match self.peek_char() {
                Some('+') => self.bump(),
                Some('-') => {
                    self.bump();
                    sign = -1;
                }
                _ => {}
            }
            let exp_digits = self.scan_ascii_digits();
            if exp_digits.is_empty() {
                self.emit_error(QueryLexError::IncompleteExponent {
                    span: self.span_from(start),
                });
            } else {
                match exp_digits.parse::<i128>() {
                    Ok(n) => {
                        has_exp = true;
                        exp = sign * n;
                    }
                    Err(_) => {
                        self.emit_error(QueryLexError::DecLiteralOverflow {
                            span: self.span_from(start),
                        });
                        return self.advance();
                    }
                }
            }
        }

        if self.peek_char() == Some('.')
            && matches!(self.peek_char2(), Some(c) if c.is_ascii_digit())
        {
            self.emit_error(QueryLexError::UnexpectedDotInNumber {
                span: self.span_current(),
            });
        }

        if self.peek_char() == Some('f') {
            let raw = &self.source[start..self.pos];
            self.bump();
            let value = raw.parse::<f64>().unwrap_or(f64::NAN);
            return self.make_token(start, TokenKind::FloatLit(value));
        }

        if frac_digits.is_empty() && !has_exp {
            match int_digits.parse::<i64>() {
                Ok(n) => return self.make_token(start, TokenKind::IntLit(n)),
                Err(_) => {
                    self.emit_error(QueryLexError::IntLiteralOverflow {
                        span: self.span_from(start),
                    });
                    return self.advance();
                }
            }
        }

        self.scan_decimal(start, &int_digits, &frac_digits, exp)
    }

    fn scan_decimal(
        &mut self,
        start: usize,
        int_digits: &str,
        frac_digits: &str,
        exp: i128,
    ) -> Token {
        let mut combined = String::with_capacity(int_digits.len() + frac_digits.len());
        combined.push_str(int_digits);
        combined.push_str(frac_digits);
        let trimmed = match combined.trim_start_matches('0') {
            "" => "0",
            t => t,
        };

        let mut scale: i128 = frac_digits.len() as i128 - exp;

        let mut warned = false;
        let mut digits: String = if scale < 0 {
            let extra = (-scale) as usize;
            if trimmed.len() + extra > 29 {
                self.emit_error(QueryLexError::DecLiteralOverflow {
                    span: self.span_from(start),
                });
                return self.advance();
            }
            let mut s = String::with_capacity(trimmed.len() + extra);
            s.push_str(trimmed);
            s.push_str(&"0".repeat(extra));
            scale = 0;
            s
        } else if scale > 28 {
            let kept_len = trimmed.len().saturating_sub((scale - 28) as usize);
            let (kept, dropped) = trimmed.split_at(kept_len);
            let round_up = should_round_up(dropped, kept);
            let rounded = if round_up {
                increment_digits(kept)
            } else if kept.is_empty() {
                "0".to_string()
            } else {
                kept.to_string()
            };
            if dropped.bytes().any(|b| b != b'0') {
                warned = true;
            }
            scale = 28;
            rounded
        } else {
            trimmed.to_string()
        };

        loop {
            while scale > 0 && digits.len() > 1 && digits.ends_with('0') {
                digits.pop();
                scale -= 1;
            }

            if digits.len() <= 29 {
                let mantissa = digits.parse::<i128>().unwrap_or(i128::MAX);
                if mantissa <= Decimal::MAX.mantissa() {
                    if warned {
                        self.emit_warning(QueryLexError::DecimalRounded {
                            span: self.span_from(start),
                        });
                    }
                    match Decimal::try_from_i128_with_scale(mantissa, scale as u32) {
                        Ok(d) => return self.make_token(start, TokenKind::DecLit(d.normalize())),
                        Err(_) => {
                            self.emit_error(QueryLexError::DecLiteralOverflow {
                                span: self.span_from(start),
                            });
                            return self.advance();
                        }
                    }
                }
            }

            if scale == 0 {
                self.emit_error(QueryLexError::DecLiteralOverflow {
                    span: self.span_from(start),
                });
                return self.advance();
            }

            let (kept, dropped) = digits.split_at(digits.len() - 1);
            digits = if should_round_up(dropped, kept) {
                increment_digits(kept)
            } else if kept.is_empty() {
                "0".to_string()
            } else {
                kept.to_string()
            };
            scale -= 1;
            warned = true;
        }
    }

    pub(super) fn scan_ascii_digits(&mut self) -> String {
        let start = self.pos;
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_digit() {
                self.bump();
            } else {
                break;
            }
        }
        self.source[start..self.pos].to_string()
    }
}

pub(super) fn should_round_up(dropped: &str, kept: &str) -> bool {
    if dropped.is_empty() {
        return false;
    }
    for (i, &b) in dropped.as_bytes().iter().enumerate() {
        let half_digit = if i == 0 { b'5' } else { b'0' };
        if b > half_digit {
            return true;
        }
        if b < half_digit {
            return false;
        }
    }
    matches!(
        kept.chars().next_back(),
        Some(c) if c.to_digit(10).is_some_and(|d| d % 2 == 1)
    )
}

fn increment_digits(s: &str) -> String {
    let mut chars: Vec<char> = s.chars().collect();
    for i in (0..chars.len()).rev() {
        if chars[i] == '9' {
            chars[i] = '0';
        } else {
            chars[i] = char::from_digit(chars[i].to_digit(10).unwrap() + 1, 10).unwrap();
            return chars.into_iter().collect();
        }
    }
    let mut out = String::with_capacity(chars.len() + 1);
    out.push('1');
    out.extend(chars);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tests::{assert_eof, assert_ident, assert_token};
    use rust_decimal::dec;

    #[test]
    fn int_literals() {
        let mut lex = Lexer::new("42 0 007");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::IntLit(42));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::IntLit(0));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::IntLit(7));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn int_overflow_error() {
        let mut lex = Lexer::new("9223372036854775808");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_literals() {
        let mut lex = Lexer::new("3.14 42.0");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(3.14)));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(42.0)));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_exponents() {
        let mut lex = Lexer::new("1e10 1e-3 123.456e2 123.456e-2 1E5");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(10000000000)));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(0.001)));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(12345.6)));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(1.23456)));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(100000)));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn float_literals() {
        let mut lex = Lexer::new("4.14f 42f 1e10f 0.5f");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::FloatLit(4.14));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::FloatLit(42.0));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::FloatLit(1e10));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::FloatLit(0.5));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn float_infinity() {
        let mut lex = Lexer::new("1e400f");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::FloatLit(f64::INFINITY));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn incomplete_exponent_error() {
        let mut lex = Lexer::new("1e");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::IntLit(1));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn incomplete_exponent_with_sign_error() {
        let mut lex = Lexer::new("1.5e+");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(1.5)));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn double_dot_in_number_error() {
        let mut lex = Lexer::new("1.2.3");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(1.2)));
        assert_token!(lex, TokenKind::Dot);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::IntLit(3));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn number_stops_at_method_dot() {
        let mut lex = Lexer::new("1.to_string() 3.14.to_string()");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::IntLit(1));
        assert_token!(lex, TokenKind::Dot);
        assert_ident!(lex, "to_string");
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit(dec!(3.14)));
        assert_token!(lex, TokenKind::Dot);
        assert_ident!(lex, "to_string");
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_rounding_half_even_down() {
        let mut lex = Lexer::new("70000000000000000000000000000.5000000000000000000000000000");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit(dec!(70000000000000000000000000000))
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_rounding_half_even_up() {
        let mut lex = Lexer::new("1.00000000000000000000000000015");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit(dec!(1.0000000000000000000000000002))
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_rounding_carry() {
        let mut lex = Lexer::new("9.99999999999999999999999999995");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit(dec!(10.000000000000000000000000000))
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_underflow() {
        let mut lex = Lexer::new("1e-29");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit(dec!(0.0000000000000000000000000000))
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_overflow_error() {
        let mut lex = Lexer::new("99999999999999999999999999999");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
        let mut lex = Lexer::new("1e29");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_max() {
        let mut lex = Lexer::new("79228162514264337593543950335.0");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit(dec!(79228162514264337593543950335))
        );
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_scale_28() {
        let mut lex = Lexer::new("0.0000000000000000000000000001");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit(dec!(0.0000000000000000000000000001))
        );
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_trailing_zeros_normalized() {
        let mut lex = Lexer::new("42.0 1.2300 0.0000000000000000000000000001");
        let TokenKind::DecLit(d) = lex.next_token().value else {
            panic!("expected DecLit");
        };
        assert_eq!(d, "42".parse().unwrap());
        assert_eq!(d.scale(), 0);
        let TokenKind::DecLit(d) = lex.next_token().value else {
            panic!("expected DecLit");
        };
        assert_eq!(d, "1.23".parse().unwrap());
        assert_eq!(d.scale(), 2);
        let TokenKind::DecLit(d) = lex.next_token().value else {
            panic!("expected DecLit");
        };
        assert_eq!(d, "0.0000000000000000000000000001".parse().unwrap());
        assert_eq!(d.scale(), 28);
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_overflow_integer_part_too_large() {
        let mut lex = Lexer::new("99999999999999999999999999999.5");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }
}
