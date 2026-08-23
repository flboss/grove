use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use rust_decimal::Decimal;

use crate::error::QueryLexError;
use crate::token::{Token, TokenKind};
use grove_types::{Diagnostic, Span, Spanned};

pub struct Lexer<'src> {
    source: &'src str,
    pos: usize,
    peeked: Option<Token>,
    peeked2: Option<Token>,
    diagnostics: Vec<Diagnostic>,
}

impl<'src> Lexer<'src> {
    pub fn new(source: &'src str) -> Self {
        Lexer {
            source,
            pos: 0,
            peeked: None,
            peeked2: None,
            diagnostics: Vec::new(),
        }
    }

    pub fn next_token(&mut self) -> Token {
        if let Some(tok) = self.peeked.take() {
            self.peeked = self.peeked2.take();
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

    pub fn peek2(&mut self) -> &Token {
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
                '.' => self.make_token(start, TokenKind::Dot),
                '+' => self.make_token(start, TokenKind::Plus),
                '-' => self.make_token(start, TokenKind::Minus),
                '*' => self.make_token(start, TokenKind::Star),
                '%' => self.make_token(start, TokenKind::Percent),
                '/' => self.make_token(start, TokenKind::Slash),
                '=' => self.scan_pair(start, '=', TokenKind::EqualsEquals, TokenKind::Equals),
                '!' => self.scan_pair(start, '=', TokenKind::BangEquals, TokenKind::Bang),
                '<' => self.scan_pair(start, '=', TokenKind::LessEquals, TokenKind::Less),
                '>' => self.scan_pair(start, '=', TokenKind::GreaterEquals, TokenKind::Greater),
                '&' => self.scan_amp_or_error(start),
                '|' => self.scan_pipe_or_error(start),
                '?' => self.scan_question_or_error(start),
                ':' => self.scan_colon_or_error(start),
                '"' => self.scan_string(start),
                '`' => self.scan_backtick_ident(start),
                '0'..='9' => return self.scan_number(start),
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

    fn scan_ident_or_keyword(&mut self, start: usize) -> Token {
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_alphanumeric() || ch == '_' {
                self.bump();
            } else {
                break;
            }
        }

        let word = &self.source[start..self.pos];
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
            "Int" => TokenKind::Int,
            "Float" => TokenKind::Float,
            "Dec" => TokenKind::Dec,
            _ => TokenKind::Ident(word.to_string()),
        };

        self.make_token(start, kind)
    }

    fn scan_pair(
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

    fn scan_amp_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('&') {
            self.bump();
            return self.make_token(start, TokenKind::AmpAmp);
        }
        self.emit_error(QueryLexError::UnexpectedAmpersand {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_pipe_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('|') {
            self.bump();
            return self.make_token(start, TokenKind::PipePipe);
        }
        self.emit_error(QueryLexError::UnexpectedPipe {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_question_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some('.') {
            self.bump();
            return self.make_token(start, TokenKind::QuestionDot);
        }
        self.emit_error(QueryLexError::UnexpectedQuestion {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_colon_or_error(&mut self, start: usize) -> Token {
        if self.peek_char() == Some(':') {
            self.bump();
            return self.make_token(start, TokenKind::DoubleColon);
        }
        self.emit_error(QueryLexError::UnexpectedColon {
            span: self.span_from(start),
        });
        self.advance()
    }

    fn scan_number(&mut self, start: usize) -> Token {
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

    fn scan_ascii_digits(&mut self) -> String {
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

    fn scan_instant(&mut self, start: usize) -> Token {
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
            self.emit_error(QueryLexError::InstantDayInvalid {
                span: self.span_from(day_start),
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

    fn fraction_nanos(&mut self, frac_start: usize, frac: &str) -> (u32, bool) {
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

    fn scan_duration(&mut self, start: usize) -> Token {
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

    fn scan_backtick_ident(&mut self, start: usize) -> Token {
        let content_start = self.pos;
        loop {
            match self.peek_char() {
                None | Some('\n') => {
                    self.emit_error(QueryLexError::UnclosedBacktickIdent {
                        span: self.span_from(start),
                    });
                    let content = self.source[content_start..self.pos].to_string();
                    return self.make_token(start, TokenKind::Ident(content));
                }
                Some('`') => {
                    let content = self.source[content_start..self.pos].to_string();
                    self.bump();
                    return self.make_token(start, TokenKind::Ident(content));
                }
                Some(_) => self.bump(),
            }
        }
    }

    fn scan_string(&mut self, start: usize) -> Token {
        let mut content = String::new();
        loop {
            match self.peek_char() {
                None | Some('\n') => {
                    self.emit_error(QueryLexError::UnclosedString {
                        span: self.span_from(start),
                    });
                    break;
                }
                Some('"') => {
                    self.bump();
                    break;
                }
                Some('\\') => {
                    self.scan_escape(&mut content);
                }
                Some(ch) => {
                    self.bump();
                    content.push(ch);
                }
            }
        }
        self.make_token(start, TokenKind::StringLit(content))
    }

    fn scan_escape(&mut self, content: &mut String) {
        let start = self.pos;
        self.bump();
        let Some(ch) = self.peek_char() else {
            return;
        };
        match ch {
            'n' => {
                self.bump();
                content.push('\n');
            }
            't' => {
                self.bump();
                content.push('\t');
            }
            'r' => {
                self.bump();
                content.push('\r');
            }
            '\\' => {
                self.bump();
                content.push('\\');
            }
            '"' => {
                self.bump();
                content.push('"');
            }
            '0' => {
                self.bump();
                content.push('\0');
            }
            'x' => {
                self.bump();
                self.scan_hex_escape(content);
            }
            'u' => {
                self.bump();
                self.scan_unicode_escape(content);
            }
            _ => {
                self.bump();
                self.emit_error(QueryLexError::UnknownEscape {
                    span: self.span_from(start),
                    ch,
                });
            }
        }
    }

    fn scan_hex_escape(&mut self, content: &mut String) {
        let start = self.pos;
        let mut value = 0u32;
        for _ in 0..2 {
            match self.peek_char() {
                Some(d) if d.is_ascii_hexdigit() => {
                    value = value * 16 + d.to_digit(16).unwrap();
                    self.bump();
                }
                _ => {
                    self.emit_error(QueryLexError::HexEscapeInvalid {
                        span: self.span_from(start),
                    });
                    return;
                }
            }
        }
        if value >= 0x80 {
            self.emit_error(QueryLexError::HexEscapeNonAscii {
                span: self.span_from(start),
                value: value as u8,
            });
        } else {
            content.push(char::from_u32(value).unwrap());
        }
    }

    fn scan_unicode_escape(&mut self, content: &mut String) {
        let start = self.pos;

        match self.peek_char() {
            Some('{') => self.bump(),
            None | Some('\n') => return,
            Some(_) => {
                let span = self.span_from(start);
                self.emit_error(QueryLexError::UnicodeEscapeExpectedLBrace { span });
                return;
            }
        }

        let mut value = 0u32;
        let mut digits = 0u32;
        let mut invalid = false;
        loop {
            match self.peek_char() {
                Some('}') => {
                    self.bump();
                    break;
                }
                None | Some('\n') => return,
                Some(d) if d.is_ascii_hexdigit() => {
                    if digits < 6 {
                        value = value * 16 + d.to_digit(16).unwrap();
                    }
                    digits += 1;
                    self.bump();
                }
                Some(d) => {
                    let start = self.pos;
                    self.bump();
                    self.emit_error(QueryLexError::UnicodeEscapeInvalidDigit {
                        span: self.span_from(start),
                        ch: d,
                    });
                    invalid = true;
                }
            }
        }

        if invalid {
            return;
        }
        if digits > 6 {
            self.emit_error(QueryLexError::UnicodeEscapeTooManyDigits {
                span: self.span_from(start),
            });
            return;
        }
        if digits == 0 {
            self.emit_error(QueryLexError::UnicodeEscapeEmpty {
                span: self.span_from(start),
            });
            return;
        }
        match char::from_u32(value) {
            Some(c) => content.push(c),
            None => {
                self.emit_error(QueryLexError::UnicodeEscapeInvalidScalarValue {
                    span: self.span_from(start),
                    value,
                });
            }
        }
    }

    fn skip_whitespace_and_comments(&mut self) {
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
        Span {
            start,
            end: self.pos,
        }
    }

    fn span_current(&self) -> Span {
        let len = self.peek_char().map(char::len_utf8).unwrap_or(0);
        Span {
            start: self.pos,
            end: self.pos + len,
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

    fn emit_error(&mut self, err: QueryLexError) {
        self.diagnostics.push(err.into());
    }

    fn emit_warning(&mut self, err: QueryLexError) {
        self.diagnostics.push(err.into());
    }
}

fn should_round_up(dropped: &str, kept: &str) -> bool {
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
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn whitespace_is_skipped() {
        let mut lex = Lexer::new("  \n\t root  \r\n");
        assert_ident!(lex, "root");
        assert_eof!(lex);
    }

    #[test]
    fn line_comments_are_skipped() {
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
    fn block_comment_is_skipped() {
        let mut lex = Lexer::new("/* hi */ users");
        assert_ident!(lex, "users");
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn block_comment_spans_lines() {
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

    #[test]
    fn keywords() {
        let mut lex = Lexer::new("none some true false prev in as if else mod asc desc");
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
        assert_eof!(lex);
    }

    #[test]
    fn type_keywords() {
        let mut lex = Lexer::new("Int Float Dec");
        assert_token!(lex, TokenKind::Int);
        assert_token!(lex, TokenKind::Float);
        assert_token!(lex, TokenKind::Dec);
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
        let mut lex = Lexer::new("incoming truex Intx");
        assert_ident!(lex, "incoming");
        assert_ident!(lex, "truex");
        assert_ident!(lex, "Intx");
        assert_eof!(lex);
    }

    #[test]
    fn backtick_ident_escapes_keywords() {
        let mut lex = Lexer::new("`prev` `true` `foo`");
        assert_ident!(lex, "prev");
        assert_ident!(lex, "true");
        assert_ident!(lex, "foo");
        assert_eof!(lex);
    }

    #[test]
    fn unclosed_backtick() {
        let mut lex = Lexer::new("`prev");
        assert_ident!(lex, "prev");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn string_literal() {
        let mut lex = Lexer::new(r#""hello world""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("hello world".into()));
        assert_eof!(lex);
    }

    #[test]
    fn string_escapes() {
        let mut lex = Lexer::new(r#""a\n\t\r\\\"\0""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("a\n\t\r\\\"\0".into()));
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn string_hex_and_unicode_escapes() {
        let mut lex = Lexer::new(r#""\x41\u{1F600}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("A\u{1F600}".into()));
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn unclosed_string() {
        let mut lex = Lexer::new(r#""hello"#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("hello".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn string_cannot_span_lines() {
        let mut lex = Lexer::new("\"abc\ndef\"");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("abc".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn non_ascii_hex_escape_is_error() {
        let mut lex = Lexer::new(r#""\x80""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn invalid_hex_escape_is_error() {
        let mut lex = Lexer::new(r#""\xG1""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("G1".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unknown_escape_is_error() {
        let mut lex = Lexer::new(r#""\z""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_empty() {
        let mut lex = Lexer::new(r#""\u{}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_missing_lbrace() {
        let mut lex = Lexer::new(r#""\u41""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("41".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_invalid_digit() {
        let mut lex = Lexer::new(r#""\u{41x}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_too_many_digits() {
        let mut lex = Lexer::new(r#""\u{1234567}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_out_of_range() {
        let mut lex = Lexer::new(r#""\u{110000}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn unicode_escape_surrogate() {
        let mut lex = Lexer::new(r#""\u{D800}""#);
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::StringLit("".into()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn punctuation() {
        let mut lex = Lexer::new("{}[](),.+*%-");
        assert_token!(lex, TokenKind::LBrace);
        assert_token!(lex, TokenKind::RBrace);
        assert_token!(lex, TokenKind::LBracket);
        assert_token!(lex, TokenKind::RBracket);
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        assert_token!(lex, TokenKind::Comma);
        assert_token!(lex, TokenKind::Dot);
        assert_token!(lex, TokenKind::Plus);
        assert_token!(lex, TokenKind::Star);
        assert_token!(lex, TokenKind::Percent);
        assert_token!(lex, TokenKind::Minus);
        assert_eof!(lex);
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
        assert_token!(lex, TokenKind::QuestionDot);
        assert_eof!(lex);
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
    fn lone_ampersand_is_error() {
        let mut lex = Lexer::new("a & b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn lone_pipe_is_error() {
        let mut lex = Lexer::new("a | b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn lone_question_is_error() {
        let mut lex = Lexer::new("a ? b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn lone_colon_is_error() {
        let mut lex = Lexer::new("a : b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn invalid_char_is_error() {
        let mut lex = Lexer::new("a \0 b");
        assert_ident!(lex, "a");
        assert_ident!(lex, "b");
        assert_eq!(lex.take_diagnostics().len(), 1);
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
    fn span_offsets() {
        let mut lex = Lexer::new("  foo  ");
        let tok = lex.next_token();
        assert_eq!(tok.span.start, 2);
        assert_eq!(tok.span.end, 5);
        assert!(matches!(tok.value, TokenKind::Ident(_)));
    }

    #[test]
    fn span_of_keyword() {
        let mut lex = Lexer::new("in");
        let tok = lex.next_token();
        assert_eq!(tok.span.start, 0);
        assert_eq!(tok.span.end, 2);
        assert_eq!(tok.value, TokenKind::In);
    }

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
    fn int_overflow_is_error() {
        let mut lex = Lexer::new("9223372036854775808");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_literals() {
        let mut lex = Lexer::new("3.14 42.0");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("3.14".parse().unwrap()));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("42.0".parse().unwrap()));
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_exponents() {
        let mut lex = Lexer::new("1e10 1e-3 123.456e2 123.456e-2 1E5");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("10000000000".parse().unwrap()));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("0.001".parse().unwrap()));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("12345.6".parse().unwrap()));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("1.23456".parse().unwrap()));
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("100000".parse().unwrap()));
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
    fn incomplete_exponent_is_error() {
        let mut lex = Lexer::new("1e");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::IntLit(1));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn incomplete_exponent_with_sign_is_error() {
        let mut lex = Lexer::new("1.5e+");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("1.5".parse().unwrap()));
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn double_dot_in_number_is_error() {
        let mut lex = Lexer::new("1.2.3");
        let tok = lex.next_token();
        assert_eq!(tok.value, TokenKind::DecLit("1.2".parse().unwrap()));
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
        assert_eq!(tok.value, TokenKind::DecLit("3.14".parse().unwrap()));
        assert_token!(lex, TokenKind::Dot);
        assert_ident!(lex, "to_string");
        assert_token!(lex, TokenKind::LParen);
        assert_token!(lex, TokenKind::RParen);
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_bankers_rounding_round_down() {
        let mut lex = Lexer::new("1.00000000000000000000000000005");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit("1.0000000000000000000000000000".parse().unwrap())
        );
        let diags = lex.take_diagnostics();
        assert_eq!(diags.len(), 1);
        assert_eq!(diags[0].severity, grove_types::Severity::Warning);
    }

    #[test]
    fn decimal_bankers_rounding_round_up() {
        let mut lex = Lexer::new("1.00000000000000000000000000015");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit("1.0000000000000000000000000002".parse().unwrap())
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_rounding_carry() {
        let mut lex = Lexer::new("9.99999999999999999999999999995");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit("10.000000000000000000000000000".parse().unwrap())
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_underflow() {
        let mut lex = Lexer::new("1e-29");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit("0.0000000000000000000000000000".parse().unwrap())
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_overflow_exponent_is_error() {
        let mut lex = Lexer::new("1e29");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_overflow_is_error() {
        let mut lex = Lexer::new("99999999999999999999999999999");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_max() {
        let mut lex = Lexer::new("79228162514264337593543950335.0");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit("79228162514264337593543950335".parse().unwrap())
        );
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_over_max_is_error() {
        let mut lex = Lexer::new("79228162514264337593543950336e0");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_scale_28() {
        let mut lex = Lexer::new("0.0000000000000000000000000001");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit("0.0000000000000000000000000001".parse().unwrap())
        );
        assert_eof!(lex);
        assert!(lex.take_diagnostics().is_empty());
    }

    #[test]
    fn decimal_normalizes_trailing_zeros() {
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
    fn decimal_rounds_whole_fraction_away() {
        let mut lex = Lexer::new("70000000000000000000000000000.5000000000000000000000000000");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::DecLit("70000000000000000000000000000".parse().unwrap())
        );
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn decimal_overflow_integer_part_too_large() {
        let mut lex = Lexer::new("99999999999999999999999999999.5");
        assert_eof!(lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

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

    fn drain(lex: &mut Lexer) {
        while !matches!(lex.next_token().value, TokenKind::Eof) {}
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
        let mut lex = Lexer::new("@2024-02-30");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 1);
    }

    #[test]
    fn instant_invalid_hour() {
        let mut lex = Lexer::new("@2026-10-20_25:00");
        drain(&mut lex);
        assert_eq!(lex.take_diagnostics().len(), 2);
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
    fn instant_fraction_rounds_with_carry() {
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
        let mut lex = Lexer::new("@2026-10-16_14:30:00.1234567895");
        let tok = lex.next_token();
        assert_eq!(
            tok.value,
            TokenKind::InstantLit(utc(2026, 10, 16, 14, 30, 0, 123456790))
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
    fn duration_fraction_rounds_with_warning() {
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
