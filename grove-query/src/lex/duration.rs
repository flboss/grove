use chrono::TimeDelta;

use crate::lex::Lexer;
use crate::lex::error::QueryLexError;
use crate::token::{Token, TokenKind};

impl<'src> Lexer<'src> {
    pub(super) fn scan_duration(&mut self, start: usize) -> Token {
        let Ok(kind) = self.scan_duration_value(start) else {
            return self.advance();
        };
        self.make_token(start, kind)
    }

    fn scan_duration_value(&mut self, start: usize) -> Result<TokenKind, ()> {
        let mut total_secs: i64 = 0;
        let mut total_nanos: u32 = 0;
        let mut seen = [false; 6];

        while matches!(self.peek_char(), Some(c) if c.is_ascii_digit()) {
            let component_start = self.pos;
            let whole = self.scan_ascii_digits();
            let frac_start = self.pos;
            let mut frac = String::new();
            if self.peek_char() == Some('.')
                && matches!(self.peek_char2(), Some(c) if c.is_ascii_digit())
            {
                self.bump();
                frac = self.scan_ascii_digits();
            }

            let Some(unit) = self.peek_char() else {
                self.emit_error(QueryLexError::DurationUnitMissing {
                    span: self.span_from(component_start),
                });
                return Err(());
            };
            let (multiplier, unit_index): (i64, usize) = match unit {
                'y' => (365 * 24 * 60 * 60, 0),
                'w' => (7 * 24 * 60 * 60, 1),
                'd' => (24 * 60 * 60, 2),
                'h' => (60 * 60, 3),
                'm' => (60, 4),
                's' => (1, 5),
                _ => {
                    self.emit_error(QueryLexError::DurationUnitMissing {
                        span: self.span_from(component_start),
                    });
                    return Err(());
                }
            };
            self.bump();

            if seen[unit_index] {
                self.emit_error(QueryLexError::DurationUnitDuplicate {
                    span: self.span_from(component_start),
                    unit,
                });
                return Err(());
            }
            seen[unit_index] = true;

            if !frac.is_empty() && unit != 's' {
                self.emit_error(QueryLexError::DurationFractionOnNonSecond {
                    span: self.span_from(frac_start),
                });
                return Err(());
            }

            let Ok(whole_value) = whole.parse::<i64>() else {
                self.emit_error(QueryLexError::DurationOverflow {
                    span: self.span_from(component_start),
                });
                return Err(());
            };

            let mut component_secs = whole_value;
            if unit == 's' {
                let mut nanos: u32 = 0;
                let mut carry = false;
                if !frac.is_empty() {
                    (nanos, carry) = self.fraction_nanos(frac_start, &frac);
                }
                total_nanos = nanos;
                component_secs = component_secs.checked_add(carry as i64).ok_or_else(|| {
                    self.emit_error(QueryLexError::DurationOverflow {
                        span: self.span_from(component_start),
                    })
                })?;
            } else {
                component_secs = match whole_value.checked_mul(multiplier) {
                    Some(v) => v,
                    None => {
                        self.emit_error(QueryLexError::DurationOverflow {
                            span: self.span_from(component_start),
                        });
                        return Err(());
                    }
                };
            }

            total_secs = match total_secs.checked_add(component_secs) {
                Some(v) => v,
                None => {
                    self.emit_error(QueryLexError::DurationOverflow {
                        span: self.span_from(component_start),
                    });
                    return Err(());
                }
            };
        }

        if seen.iter().all(|&s| !s) {
            self.emit_error(QueryLexError::DurationExpected {
                span: self.span_current(),
            });
            return Err(());
        }

        match TimeDelta::new(total_secs, total_nanos) {
            Some(d) => Ok(TokenKind::DurationLit(d)),
            None => {
                self.emit_error(QueryLexError::DurationOverflow {
                    span: self.span_from(start),
                });
                Err(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tests::{assert_eof, assert_token, drain};

    fn assert_duration(src: &str, expected: TimeDelta) {
        let mut lex = Lexer::new(src);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DurationLit(expected), "for `{src}`");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
    }

    #[test]
    fn duration_single_units() {
        assert_duration("#30d", TimeDelta::days(30));
        assert_duration("#5m", TimeDelta::minutes(5));
        assert_duration("#1h", TimeDelta::hours(1));
        assert_duration("#0.5s", TimeDelta::milliseconds(500));
        assert_duration("#1y", TimeDelta::days(365));
        assert_duration("#1w", TimeDelta::weeks(1));
    }

    #[test]
    fn duration_compound() {
        assert_duration(
            "#1y2w3d4h5m6.789s",
            TimeDelta::days(365)
                + TimeDelta::weeks(2)
                + TimeDelta::days(3)
                + TimeDelta::hours(4)
                + TimeDelta::minutes(5)
                + TimeDelta::seconds(6)
                + TimeDelta::nanoseconds(789_000_000),
        );
    }

    #[test]
    fn duration_any_order() {
        assert_duration("#5s2m", TimeDelta::minutes(2) + TimeDelta::seconds(5));
    }

    #[test]
    fn duration_fractional_seconds_precision() {
        assert_duration(
            "#1.234567891s",
            TimeDelta::seconds(1) + TimeDelta::nanoseconds(234_567_891),
        );
    }

    #[test]
    fn duration_fraction_rounded_with_warning() {
        let mut lex = Lexer::new("#0.1234567895s");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DurationLit(TimeDelta::nanoseconds(123_456_790))
        );
        let diags = lex.take_diagnostics();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, grove_types::Severity::Warning);
    }

    #[test]
    fn duration_fraction_carry_to_seconds() {
        let mut lex = Lexer::new("#1.9999999995s");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DurationLit(TimeDelta::seconds(2)));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn duration_duplicate_unit() {
        let mut lex = Lexer::new("#1h1h");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn duration_fraction_on_non_second() {
        let mut lex = Lexer::new("#1.5m");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn duration_missing_unit() {
        let mut lex = Lexer::new("#30 #30x");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 2);
    }

    #[test]
    fn duration_expected() {
        let mut lex = Lexer::new("# #x");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 2);
    }

    #[test]
    fn duration_overflow() {
        let mut lex = Lexer::new("#99999999999999999999y");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }
}
