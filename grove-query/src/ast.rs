use std::fmt;

use chrono::{DateTime, NaiveTime, TimeDelta, Utc};
use grove_schema::validated::ScalarType;
use grove_types::{Span, Spanned};
use rust_decimal::Decimal;

#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryFile {
    pub statements: Vec<Statement>,
}

impl QueryFile {
    pub fn output(&self) -> Option<&Expr> {
        match self.statements.last() {
            Some(Statement::Expr(expr)) => Some(expr),
            _ => None,
        }
    }

    pub fn into_statements_and_output(self) -> (Vec<Statement>, Option<Expr>) {
        let mut statements = self.statements;
        let output = match statements.last() {
            Some(Statement::Expr(_)) => match statements.pop() {
                Some(Statement::Expr(expr)) => Some(expr),
                _ => unreachable!(),
            },
            _ => None,
        };
        (statements, output)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Mutation(MutationStmt),
    Let(LetStmt),
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MutationStmt {
    pub kind: Spanned<MutationKind>,
    pub base: Expr,
    pub args: Vec<Expr>,
    pub opt_depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MutationKind {
    Insert,
    Update,
    Delete,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LetStmt {
    pub name: Spanned<String>,
    pub value: Expr,
}

impl fmt::Display for MutationKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MutationKind::Insert => write!(f, "insert"),
            MutationKind::Update => write!(f, "update"),
            MutationKind::Delete => write!(f, "delete"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Spanned<Literal>),
    Ident(Spanned<String>),
    EnumVariant {
        enum_name: Spanned<String>,
        variant: Spanned<String>,
        span: Span,
    },
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
        ty: Spanned<TypeExpr>,
        fallible: bool,
        span: Span,
    },
    Field {
        base: Box<Expr>,
        name: Spanned<String>,
        opt_depth: usize,
        span: Span,
    },
    TupleAccess {
        base: Box<Expr>,
        index: Spanned<i64>,
        opt_depth: usize,
        span: Span,
    },
    Method {
        base: Box<Expr>,
        name: Spanned<String>,
        args: Vec<Arg>,
        opt_depth: usize,
        span: Span,
    },
    Projection {
        base: Box<Expr>,
        items: Vec<ProjectionItem>,
        spread: Option<Span>,
        span: Span,
    },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Literal(lit) => lit.span,
            Expr::Ident(name) => name.span,
            Expr::EnumVariant { span, .. } => *span,
            Expr::TypeConstant { ty, name } => ty.span.merge(name.span),
            Expr::Array { span, .. }
            | Expr::Tuple { span, .. }
            | Expr::Some { span, .. }
            | Expr::Struct { span, .. }
            | Expr::If { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. }
            | Expr::Cast { span, .. }
            | Expr::Field { span, .. }
            | Expr::TupleAccess { span, .. }
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
    Bytes(Vec<u8>),
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
    String,
    Bytes,
    Bool,
    Instant,
    Duration,
}

impl fmt::Display for TypeName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeName::Int => write!(f, "Int"),
            TypeName::Float => write!(f, "Float"),
            TypeName::Dec => write!(f, "Dec"),
            TypeName::String => write!(f, "String"),
            TypeName::Bytes => write!(f, "Bytes"),
            TypeName::Bool => write!(f, "Bool"),
            TypeName::Instant => write!(f, "Instant"),
            TypeName::Duration => write!(f, "Duration"),
        }
    }
}

impl From<ScalarType> for TypeName {
    fn from(value: ScalarType) -> Self {
        match value {
            ScalarType::Int => TypeName::Int,
            ScalarType::Float => TypeName::Float,
            ScalarType::Dec => TypeName::Dec,
            ScalarType::String => TypeName::String,
            ScalarType::Bytes => TypeName::Bytes,
            ScalarType::Bool => TypeName::Bool,
            ScalarType::Instant => TypeName::Instant,
            ScalarType::Duration => TypeName::Duration,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeExpr {
    Scalar(TypeName),
    Named(String),
    Optional {
        inner: Box<Spanned<TypeExpr>>,
        span: Span,
    },
    List {
        element: Box<Spanned<TypeExpr>>,
        span: Span,
    },
    Tuple {
        elements: Vec<Spanned<TypeExpr>>,
        span: Span,
    },
    Record {
        fields: Vec<(Spanned<String>, Spanned<TypeExpr>)>,
        span: Span,
    },
}

impl fmt::Display for TypeExpr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TypeExpr::Scalar(ty) => write!(f, "{ty}"),
            TypeExpr::Named(name) => write!(f, "{name}"),
            TypeExpr::Optional { inner, .. } => write!(f, "?{}", inner.value),
            TypeExpr::List { element, .. } => write!(f, "List<{}>", element.value),
            TypeExpr::Tuple { elements, .. } => {
                write!(f, "(")?;
                for (i, element) in elements.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}", element.value)?;
                }
                write!(f, ")")
            }
            TypeExpr::Record { fields, .. } => {
                write!(f, "{{")?;
                for (i, (name, ty)) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", name.value, ty.value)?;
                }
                write!(f, "}}")
            }
        }
    }
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

impl BinaryOp {
    pub fn is_arithmetic(&self) -> bool {
        matches!(
            self,
            BinaryOp::Add
                | BinaryOp::Sub
                | BinaryOp::Mul
                | BinaryOp::Div
                | BinaryOp::Rem
                | BinaryOp::Mod
        )
    }
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
