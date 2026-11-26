use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};

use super::number::should_round_up;
use crate::lex::Lexer;
use crate::lex::error::QueryLexError;
use crate::token::{Token, TokenKind};
use grove_types::Span;

impl<'src> Lexer<'src> {
    pub(super) fn scan_instant(&mut self, start: usize) -> Token {
        let kind = match self.scan_instant_value() {
            Ok(kind) => kind,
            Err(()) => return self.advance(),
        };
        if self.instant_has_stray_suffix() {
            return self.advance();
        }
        self.make_token(start, kind)
    }

    fn scan_instant_value(&mut self) -> Result<TokenKind, ()> {
        match self.peek_char() {
            Some(c) if c.is_ascii_alphabetic() => self.scan_instant_name(),
            Some(c) if c.is_ascii_digit() || c == '-' => self.scan_instant_date(),
            Some(_) => {
                let span = self.span_current();
                self.emit_error(QueryLexError::InstantExpected { span });
                self.bump();
                Err(())
            }
            None => {
                let span = self.span_current();
                self.emit_error(QueryLexError::InstantExpected { span });
                Err(())
            }
        }
    }

    fn scan_instant_name(&mut self) -> Result<TokenKind, ()> {
        let name_start = self.pos;
        let mut name = String::new();
        while let Some(c) = self.peek_char() {
            if c.is_ascii_alphabetic() {
                self.bump();
                name.push(c);
            } else {
                break;
            }
        }

        match name.as_str() {
            "now" => {
                if matches!(self.peek_char(), Some(c) if c.is_ascii_alphanumeric() || c == '_') {
                    self.emit_error(QueryLexError::InstantNameInvalid {
                        span: self.span_from(name_start),
                        name,
                    });
                    return Err(());
                }
                Ok(TokenKind::Now)
            }
            "today" => {
                if self.peek_char() == Some('_') {
                    self.bump();
                    let time_start = self.pos;
                    let (secs_of_day, nanos) = self.scan_time_part()?;
                    let Some(time) =
                        NaiveTime::from_num_seconds_from_midnight_opt(secs_of_day, nanos)
                    else {
                        self.emit_error(QueryLexError::InstantSecondInvalid {
                            span: self.span_from(time_start),
                        });
                        return Err(());
                    };
                    Ok(TokenKind::Today(Some(time)))
                } else if matches!(
                    self.peek_char(),
                    Some(c) if c.is_ascii_alphanumeric() || c == '_'
                ) {
                    self.emit_error(QueryLexError::InstantNameInvalid {
                        span: self.span_from(name_start),
                        name,
                    });
                    Err(())
                } else {
                    Ok(TokenKind::Today(None))
                }
            }
            "unix" => {
                if self.peek_char() != Some('_') {
                    self.emit_error(QueryLexError::InstantNameInvalid {
                        span: self.span_from(name_start),
                        name,
                    });
                    return Err(());
                }
                self.bump();
                let unix_seconds_start = self.pos;
                let mut sign = 1i64;
                match self.peek_char() {
                    Some('-') => {
                        self.bump();
                        sign = -1;
                    }
                    Some('+') => self.bump(),
                    _ => {}
                }
                let secs_digits = self.scan_ascii_digits();
                if secs_digits.is_empty() {
                    self.emit_error(QueryLexError::InstantUnixSecondsMissing {
                        span: self.span_from(name_start),
                    });
                    return Err(());
                }
                let mut nanos: u32 = 0;
                let mut carry = false;
                if self.peek_char() == Some('.')
                    && matches!(self.peek_char2(), Some(c) if c.is_ascii_digit())
                {
                    let frac_start = self.pos;
                    self.bump();
                    let frac = self.scan_ascii_digits();
                    (nanos, carry) = self.fraction_nanos(frac_start, &frac);
                }
                let secs = match secs_digits.parse::<i64>() {
                    Ok(n) => sign
                        .checked_mul(n)
                        .and_then(|s| s.checked_add(carry as i64)),
                    Err(_) => None,
                };
                let Some(secs) = secs else {
                    self.emit_error(QueryLexError::InstantUnixOverflow {
                        span: self.span_from(unix_seconds_start),
                    });
                    return Err(());
                };
                match DateTime::<Utc>::from_timestamp(secs, nanos) {
                    Some(dt) => Ok(TokenKind::InstantLit(dt)),
                    None => {
                        self.emit_error(QueryLexError::InstantUnixOverflow {
                            span: self.span_from(unix_seconds_start),
                        });
                        Err(())
                    }
                }
            }
            _ => {
                self.emit_error(QueryLexError::InstantNameInvalid {
                    span: self.span_from(name_start),
                    name,
                });
                Err(())
            }
        }
    }

    fn scan_instant_date(&mut self) -> Result<TokenKind, ()> {
        let mut year_sign = 1i32;
        if self.peek_char() == Some('-') {
            self.bump();
            year_sign = -1;
        }
        let year_start = self.pos;
        let year_digits = self.scan_ascii_digits();
        if year_digits.len() < 4 {
            self.emit_error(QueryLexError::InstantYearInvalid {
                span: self.span_from(year_start),
            });
            return Err(());
        }
        let year = match year_digits.parse::<i32>() {
            Ok(y) => year_sign * y,
            Err(_) => {
                self.emit_error(QueryLexError::InstantYearInvalid {
                    span: self.span_from(year_start),
                });
                return Err(());
            }
        };

        let mut month: u32 = 1;
        let mut day: u32 = 1;
        let mut day_start = 0usize;
        if self.peek_char() == Some('-') {
            self.bump();
            let month_start = self.pos;
            let Some(m) = self.scan_exact_2_digits() else {
                self.emit_error(QueryLexError::InstantMonthInvalid {
                    span: self.span_from(month_start),
                });
                return Err(());
            };
            if !(1..=12).contains(&m) {
                self.emit_error(QueryLexError::InstantMonthInvalid {
                    span: self.span_from(month_start),
                });
                return Err(());
            }
            month = m;
            if self.peek_char() == Some('-') {
                self.bump();
                day_start = self.pos;
                let Some(d) = self.scan_exact_2_digits() else {
                    self.emit_error(QueryLexError::InstantDayInvalid {
                        span: self.span_from(day_start),
                    });
                    return Err(());
                };
                day = d;
            }
        }

        let Some(date) = NaiveDate::from_ymd_opt(year, month, day) else {
            // Day 1 is valid for every month and month is already proven 1..=12,
            // so a second failure can only mean the year is out of chrono's range
            let year_bad = NaiveDate::from_ymd_opt(year, month, 1).is_none();
            self.emit_error(if year_bad {
                QueryLexError::InstantYearInvalid {
                    span: Span::new(year_start, year_start + year_digits.len()),
                }
            } else {
                QueryLexError::InstantDayInvalid {
                    span: self.span_from(day_start),
                }
            });
            return Err(());
        };

        let mut time_start = self.pos;
        let (secs_of_day, nanos) = if self.peek_char() == Some('_') {
            self.bump();
            time_start = self.pos;
            self.scan_time_part()?
        } else {
            (0, 0)
        };

        let midnight = date.and_hms_opt(0, 0, 0).unwrap();
        let Some(naive) =
            midnight.checked_add_signed(TimeDelta::new(secs_of_day as i64, nanos).unwrap())
        else {
            self.emit_error(QueryLexError::InstantSecondInvalid {
                span: self.span_from(time_start),
            });
            return Err(());
        };
        let dt = DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc);
        Ok(TokenKind::InstantLit(dt))
    }

    fn scan_time_part(&mut self) -> Result<(u32, u32), ()> {
        let hour_start = self.pos;
        let hh = match self.scan_exact_2_digits() {
            Some(d) => d,
            None => {
                self.emit_error(QueryLexError::InstantHourInvalid {
                    span: self.span_from(hour_start),
                });
                return Err(());
            }
        };
        if hh > 23 {
            self.emit_error(QueryLexError::InstantHourInvalid {
                span: self.span_from(hour_start),
            });
            return Err(());
        }
        if self.peek_char() != Some(':') {
            self.emit_error(QueryLexError::InstantTimeColonExpected {
                span: self.span_current(),
            });
            return Err(());
        }
        self.bump();
        let minute_start = self.pos;
        let mm = match self.scan_exact_2_digits() {
            Some(d) => d,
            None => {
                self.emit_error(QueryLexError::InstantMinuteInvalid {
                    span: self.span_from(minute_start),
                });
                return Err(());
            }
        };
        if mm > 59 {
            self.emit_error(QueryLexError::InstantMinuteInvalid {
                span: self.span_from(minute_start),
            });
            return Err(());
        }
        let mut secs_of_day = hh * 3600 + mm * 60;
        if self.peek_char() == Some(':') {
            self.bump();
            let second_start = self.pos;
            let ss = match self.scan_exact_2_digits() {
                Some(d) => d,
                None => {
                    self.emit_error(QueryLexError::InstantSecondInvalid {
                        span: self.span_from(second_start),
                    });
                    return Err(());
                }
            };
            if ss > 59 {
                self.emit_error(QueryLexError::InstantSecondInvalid {
                    span: self.span_from(second_start),
                });
                return Err(());
            }
            secs_of_day += ss;
            if self.peek_char() == Some('.')
                && matches!(self.peek_char2(), Some(c) if c.is_ascii_digit())
            {
                let frac_start = self.pos;
                self.bump();
                let frac = self.scan_ascii_digits();
                let (nanos, carry) = self.fraction_nanos(frac_start, &frac);
                return Ok((secs_of_day + carry as u32, nanos));
            }
        }
        Ok((secs_of_day, 0))
    }

    fn scan_exact_2_digits(&mut self) -> Option<u32> {
        let digits = self.scan_ascii_digits();
        if digits.len() != 2 {
            return None;
        }
        digits.parse::<u32>().ok()
    }

    pub(super) fn fraction_nanos(&mut self, frac_start: usize, frac: &str) -> (u32, bool) {
        let mut nanos: u32 = 0;
        for (i, ch) in frac.chars().enumerate() {
            if i == 9 {
                break;
            }
            nanos = nanos * 10 + ch.to_digit(10).unwrap();
        }
        if frac.len() < 9 {
            nanos *= 10u32.pow((9 - frac.len()) as u32);
        }
        let mut carry = false;
        if frac.len() > 9 {
            let (kept, dropped) = frac.split_at(9);
            if should_round_up(dropped, kept) {
                nanos += 1;
            }
            if dropped.bytes().any(|b| b != b'0') {
                self.emit_warning(QueryLexError::FractionRounded {
                    span: self.span_from(frac_start),
                });
            }
        }
        if nanos >= 1_000_000_000 {
            carry = true;
            nanos = 0;
        }
        (nanos, carry)
    }

    fn instant_has_stray_suffix(&mut self) -> bool {
        match self.peek_char() {
            Some(c) if c.is_ascii_alphanumeric() || c == '_' => {
                let span = self.span_current();
                self.emit_error(QueryLexError::InstantSuffixInvalid { span });
                while let Some(c) = self.peek_char() {
                    if c.is_ascii_alphanumeric() || c == '_' {
                        self.bump();
                    } else {
                        break;
                    }
                }
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lex::tests::{assert_eof, assert_ident, assert_token, drain};

    fn utc(y: i32, mo: u32, d: u32, h: u32, mi: u32, s: u32, ns: u32) -> DateTime<Utc> {
        let date = NaiveDate::from_ymd_opt(y, mo, d).unwrap();
        let time = NaiveTime::from_hms_nano_opt(h, mi, s, ns).unwrap();
        DateTime::<Utc>::from_naive_utc_and_offset(date.and_time(time), Utc)
    }

    fn assert_instant(src: &str, expected: DateTime<Utc>) {
        let mut lex = Lexer::new(src);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::InstantLit(expected), "for `{src}`");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty(), "for `{src}`");
    }

    fn assert_instant_error(src: &str, code: &str, start: usize, end: usize) {
        let mut lex = Lexer::new(src);
        drain(&mut lex);
        let diags = lex.take_diagnostics();
        assert_eq!(diags.len(), 1, "for `{src}`");
        assert_eq!(&diags[0].code[..], code, "for `{src}`");
        assert_eq!(
            diags[0].labels[0].span,
            Span::new(start, end),
            "for `{src}`"
        );
    }

    #[test]
    fn instant_date_forms() {
        assert_instant("@2026", utc(2026, 1, 1, 0, 0, 0, 0));
        assert_instant("@2026-02", utc(2026, 2, 1, 0, 0, 0, 0));
        assert_instant("@2026-02-16", utc(2026, 2, 16, 0, 0, 0, 0));
        assert_instant("@2026-02-16_14:30", utc(2026, 2, 16, 14, 30, 0, 0));
        assert_instant("@2026-02-16_14:30:00", utc(2026, 2, 16, 14, 30, 0, 0));
        assert_instant(
            "@2026-02-16_14:30:00.123456789",
            utc(2026, 2, 16, 14, 30, 0, 123456789),
        );
        assert_instant(
            "@2026-02-16_14:30:00.5",
            utc(2026, 2, 16, 14, 30, 0, 500000000),
        );
    }

    #[test]
    fn instant_leap_year() {
        assert_instant("@2024-02-29", utc(2024, 2, 29, 0, 0, 0, 0));
    }

    #[test]
    fn instant_year_time() {
        assert_instant("@2024_14:30", utc(2024, 1, 1, 14, 30, 0, 0));
    }

    #[test]
    fn instant_negative_and_long_years() {
        assert_instant("@-0044", utc(-44, 1, 1, 0, 0, 0, 0));
        assert_instant("@-10000", utc(-10000, 1, 1, 0, 0, 0, 0));
        assert_instant("@20240", utc(20240, 1, 1, 0, 0, 0, 0));
        assert_instant("@-2024-01-15", utc(-2024, 1, 15, 0, 0, 0, 0));
    }

    #[test]
    fn instant_named_forms() {
        let mut lex = Lexer::new("@now @today");
        assert_token!(lex, TokenKind::Now);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::Today(None));
        assert_eof!(lex);
    }

    #[test]
    fn instant_today_with_time() {
        let mut lex = Lexer::new("@today_08:52:00.000");
        let tok = lex.next_token();
        let expect = NaiveTime::from_hms_opt(8, 52, 0).unwrap();
        assert_eq!(tok.value, TokenKind::Today(Some(expect)));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn instant_unix_epoch() {
        assert_instant(
            "@unix_1767225600",
            DateTime::<Utc>::from_timestamp(1767225600, 0).unwrap(),
        );
    }

    #[test]
    fn instant_unix_fraction() {
        assert_instant(
            "@unix_1767225600.123",
            DateTime::<Utc>::from_timestamp(1767225600, 123000000).unwrap(),
        );
    }

    #[test]
    fn instant_unix_signed() {
        assert_instant(
            "@unix_-123.456",
            DateTime::<Utc>::from_timestamp(-123, 456000000).unwrap(),
        );
        assert_instant(
            "@unix_+123",
            DateTime::<Utc>::from_timestamp(123, 0).unwrap(),
        );
    }

    #[test]
    fn instant_stops_at_method_dot() {
        let mut lex = Lexer::new("@now.year() @2024-01-15.year()");
        assert_token!(lex, TokenKind::Now);
        assert_token!(lex, TokenKind::Dot);
        assert_ident!(lex, "year");
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::InstantLit(utc(2024, 1, 15, 0, 0, 0, 0))
        );
        assert_token!(lex, TokenKind::Dot);
        assert_ident!(lex, "year");
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn instant_invalid_month() {
        let mut lex = Lexer::new("@2026-13");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_invalid_day() {
        assert_instant_error("@2024-02-30", "QL0026", 9, 11);
        assert_instant_error("@2023-02-29", "QL0026", 9, 11);
        assert_instant_error("@2024-01-00", "QL0026", 9, 11);
    }

    #[test]
    fn instant_year_out_of_range() {
        assert_instant_error("@300000-01-01", "QL0024", 1, 7);
        assert_instant_error("@262143-06-15", "QL0024", 1, 7);
        assert_instant_error("@-300000-06-15", "QL0024", 2, 8);
        assert_instant_error("@-262144-01-01", "QL0024", 2, 8);
    }

    #[test]
    fn instant_year_range_boundaries() {
        use chrono::Datelike;
        let (lo, hi) = (NaiveDate::MIN.year(), NaiveDate::MAX.year());
        assert_instant(&format!("@{hi}-12-31"), utc(hi, 12, 31, 0, 0, 0, 0));
        assert_instant(&format!("@{lo}-01-01"), utc(lo, 1, 1, 0, 0, 0, 0));
        let over = format!("@{}-01-01", hi + 1);
        assert_instant_error(&over, "QL0024", 1, 1 + (hi + 1).to_string().len());
        let under = format!("@{}-01-01", lo - 1);
        let digits = (lo - 1).unsigned_abs().to_string().len();
        assert_instant_error(&under, "QL0024", 2, 2 + digits);
    }

    #[test]
    fn instant_invalid_hour() {
        let mut lex = Lexer::new("@2026-10-20_25:00");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_invalid_minute() {
        let mut lex = Lexer::new("@2026-10-20_12:60");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_invalid_second() {
        let mut lex = Lexer::new("@2026-10-20_14:30:60");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_missing_time_colon() {
        let mut lex = Lexer::new("@2026-10-20_1430");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_invalid_name() {
        let mut lex = Lexer::new("@foo");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_name_with_suffix() {
        let mut lex = Lexer::new("@todayx");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_unix_missing_seconds() {
        let mut lex = Lexer::new("@unix_");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_unix_overflow() {
        let mut lex = Lexer::new("@unix_99999999999999999999");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_stray_suffix() {
        let mut lex = Lexer::new("@2024x");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_short_year() {
        let mut lex = Lexer::new("@202 @-1");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 2);
    }

    #[test]
    fn instant_fraction_rounded_with_carry() {
        let mut lex = Lexer::new("@2026-10-16_14:30:00.9999999995");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::InstantLit(utc(2026, 10, 16, 14, 30, 1, 0))
        );
        let diags = lex.take_diagnostics();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, grove_types::Severity::Warning);
    }

    #[test]
    fn instant_fraction_rounding_day_rollover() {
        let mut lex = Lexer::new("@2026-10-16_23:59:59.9999999995");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::InstantLit(utc(2026, 10, 17, 0, 0, 0, 0))
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_fraction_bankers_half_even() {
        let mut lex = Lexer::new("@2026-10-16_14:30:00.1234567885");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::InstantLit(utc(2026, 10, 16, 14, 30, 0, 123456788))
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_unix_fraction_rounding_carry() {
        let mut lex = Lexer::new("@unix_1.9999999995");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::InstantLit(DateTime::<Utc>::from_timestamp(2, 0).unwrap())
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }
}
