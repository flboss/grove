use std::fmt;

use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use grove_types::{Span, Spanned};
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq)]
pub struct QueryFile {
    pub statements: Vec<Statement>,
    pub result: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Mutation(MutationStmt),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MutationStmt {
    pub kind: Spanned<MutationKind>,
    pub base: Expr,
    pub arg: Option<Box<Expr>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Spanned<Literal>),
    Ident(Spanned<String>),
    Array {
        elements: Vec<Expr>,
        span: Span,
    },
    Tuple {
        elements: Vec<Expr>,
        span: Span,
    },
    Some {
        value: Box<Expr>,
        span: Span,
    },
    TypeConstant {
        ty: Spanned<TypeName>,
        name: Spanned<ConstantName>,
    },
    Struct {
        fields: Vec<(Spanned<String>, Expr)>,
        span: Span,
    },
    If {
        arms: Vec<(Expr, Expr)>,
        default: Box<Expr>,
        span: Span,
    },
    Unary {
        op: Spanned<UnaryOp>,
        expr: Box<Expr>,
        span: Span,
    },
    Binary {
        op: Spanned<BinaryOp>,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        span: Span,
    },
    Cast {
        expr: Box<Expr>,
        ty: Spanned<TypeName>,
        span: Span,
    },
    Field {
        base: Box<Expr>,
        name: Spanned<String>,
        optional: bool,
        span: Span,
    },
    Method {
        base: Box<Expr>,
        name: Spanned<String>,
        args: Vec<Arg>,
        optional: bool,
        span: Span,
    },
    Projection {
        base: Box<Expr>,
        items: Vec<ProjectionItem>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Literal(lit) => lit.span,
            Expr::Ident(name) => name.span,
            Expr::TypeConstant { ty, name } => Span {
                start: ty.span.start,
                end: name.span.end,
            },
            Expr::Array { span, .. }
            | Expr::Tuple { span, .. }
            | Expr::Some { span, .. }
            | Expr::Struct { span, .. }
            | Expr::If { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Field { span, .. }
            | Expr::Method { span, .. }
            | Expr::Projection { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    Int(i64),
    Float(f64),
    Dec(Decimal),
    String(String),
    Bool(bool),
    Instant(DateTime<Utc>),
    Now,
    Today(Option<NaiveTime>),
    Duration(TimeDelta),
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeName {
    Int,
    Float,
    Dec,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConstantName {
    Min,
    Max,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Neg,
    Not,
}

impl fmt::Display for UnaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            UnaryOp::Neg => write!(f, "-"),
            UnaryOp::Not => write!(f, "!"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Mod,
    Lt,
    Gt,
    Le,
    Ge,
    Eq,
    Ne,
    In,
    And,
    Or,
}

impl fmt::Display for BinaryOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryOp::Add => write!(f, "+"),
            BinaryOp::Sub => write!(f, "-"),
            BinaryOp::Mul => write!(f, "*"),
            BinaryOp::Div => write!(f, "/"),
            BinaryOp::Rem => write!(f, "%"),
            BinaryOp::Mod => write!(f, "mod"),
            BinaryOp::Lt => write!(f, "<"),
            BinaryOp::Gt => write!(f, ">"),
            BinaryOp::Le => write!(f, "<="),
            BinaryOp::Ge => write!(f, ">="),
            BinaryOp::Eq => write!(f, "=="),
            BinaryOp::Ne => write!(f, "!="),
            BinaryOp::In => write!(f, "in"),
            BinaryOp::And => write!(f, "&&"),
            BinaryOp::Or => write!(f, "||"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    pub direction: Option<Spanned<SortDir>>,
    pub expr: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionItem {
    pub alias: Option<Spanned<String>>,
    pub value: Expr,
}
