use grove_types::Spanned;

pub type Token = Spanned<TokenKind>;

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    // Keywords
    Config,
    Root,
    Struct,
    Rel,
    Via,
    List,
    Tuple,
    Int,
    Float,
    Dec,
    String,
    Bool,
    Instant,
    Duration,

    // Literals
    Ident(String),
    StringLit(String),

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
    Comma,
    Dot,
    At,
    Question,
    Arrow,

    // Relation arrows
    OneToOne,
    ManyToOne,
    OneToMany,
    ManyToMany,

    Eof,
}
