use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use grove_types::Spanned;
use rust_decimal::Decimal;

pub type Token = Spanned<TokenKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    None,
    Some,
    True,
    False,
    Prev,
    In,
    As,
    If,
    Else,
    Mod,
    Asc,
    Desc,
    Int,
    Float,
    Dec,
    Bool,
    String,

    // Literals
    Ident(String),
    StringLit(String),
    IntLit(i64),
    FloatLit(f64),
    DecLit(Decimal),
    InstantLit(DateTime<Utc>),
    Now,
    Today(Option<NaiveTime>),
    DurationLit(TimeDelta),

    // Punctuation & operators
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    Comma,
    Dot,
    QuestionDot,
    Equals,
    EqualsEquals,
    Bang,
    BangEquals,
    Less,
    LessEquals,
    Greater,
    GreaterEquals,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AmpAmp,
    PipePipe,
    DoubleColon,

    Eof,
}
