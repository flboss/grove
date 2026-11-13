use grove_types::Spanned;

pub type Token = Spanned<TokenKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Config,
    Root,
    Struct,
    Enum,
    Rel,
    Via,
    List,
    Tuple,
    Int,
    Float,
    Dec,
    String,
    Bytes,
    Bool,
    Instant,
    Duration,

    // Literals
    Ident(String),
    StringLit(String),
    IntLit(i64),

    // Punctuation
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    LParen,
    RParen,
    LAngle,
    RAngle,
    Colon,
    Semicolon,
    Equals,
    FatArrow,
    Comma,
    Dot,
    At,
    Question,
    Ampersand,
    Arrow,

    // Relation arrows
    OneToOne,
    ManyToOne,
    OneToMany,
    ManyToMany,

    Eof,
}
