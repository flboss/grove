use std::fmt;

use crate::ast::{BinaryOp, ConstantName, Literal, SortDir, TypeName, UnaryOp};
use grove_schema::validated::{FieldId, ScalarType, StructId, ValueType};
use grove_types::{Span, Spanned};

#[derive(Debug, Clone, PartialEq)]
pub struct TypedQueryFile {
    pub statements: Vec<TypedStatement>,
    pub result: TypedExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedStatement {
    Mutation(TypedMutationStmt),
    Expr(TypedExpr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedMutationStmt {
    Insert {
        base: TypedExpr,
        arg: TypedExpr,
        span: Span,
    },
    Update {
        base: TypedExpr,
        arg: TypedExpr,
        span: Span,
    },
    Delete {
        base: TypedExpr,
        span: Span,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedExpr {
    pub kind: TypedExprKind,
    pub ty: QueryType,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedExprKind {
    Literal(Spanned<Literal>),
    Ident {
        name: Spanned<String>,
        binding: IdentBinding,
        depth: usize,
    },
    Field {
        base: Box<TypedExpr>,
        name: Spanned<String>,
        optional: bool,
    },
    Method {
        base: Box<TypedExpr>,
        name: Spanned<String>,
        args: Vec<TypedMethodArg>,
        optional: bool,
    },
    Binary {
        op: Spanned<BinaryOp>,
        lhs: Box<TypedExpr>,
        rhs: Box<TypedExpr>,
    },
    Unary {
        op: Spanned<UnaryOp>,
        expr: Box<TypedExpr>,
    },
    Cast {
        expr: Box<TypedExpr>,
        ty: Spanned<TypeName>,
    },
    If {
        arms: Vec<(TypedExpr, TypedExpr)>,
        default: Box<TypedExpr>,
    },
    Some {
        value: Box<TypedExpr>,
    },
    Tuple {
        elements: Vec<TypedExpr>,
    },
    Array {
        elements: Vec<TypedExpr>,
    },
    Struct {
        fields: Vec<(Spanned<String>, TypedExpr)>,
    },
    Projection {
        base: Box<TypedExpr>,
        items: Vec<TypedProjectionItem>,
    },
    TypeConstant {
        ty: Spanned<TypeName>,
        name: Spanned<ConstantName>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedProjectionItem {
    pub alias: Spanned<String>,
    pub value: TypedExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedMethodArg {
    pub direction: Option<Spanned<SortDir>>,
    pub expr: TypedExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum IdentBinding {
    Root { root_idx: usize },
    Field(FieldId),
    ProjectedField { name: String },
    Prev { struct_id: StructId },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionField {
    pub name: String,
    pub ty: QueryType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RecordSource {
    Schema(StructId),
    Projection(Vec<ProjectionField>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryType {
    Scalar(ScalarType),
    Optional(Box<QueryType>),
    List(Box<QueryType>),
    Tuple(Vec<QueryType>),
    Record(RecordSource),
    Void,
    Unknown,
}

impl QueryType {
    pub fn is_numeric(&self) -> bool {
        matches!(
            self,
            QueryType::Scalar(ScalarType::Int | ScalarType::Float | ScalarType::Dec)
        )
    }

    pub fn is_bool(&self) -> bool {
        matches!(self, QueryType::Scalar(ScalarType::Bool))
    }

    pub fn has_defined_order(&self) -> bool {
        match self {
            QueryType::Scalar(_) => true,
            QueryType::Tuple(elems) => elems.iter().all(Self::has_defined_order),
            _ => false,
        }
    }

    pub fn is_summable(&self) -> bool {
        self.is_numeric() || matches!(self, QueryType::Scalar(ScalarType::Duration))
    }

    pub fn is_optional(&self) -> bool {
        matches!(self, QueryType::Optional(_))
    }

    pub fn unwrap_optional(&self) -> &QueryType {
        match self {
            QueryType::Optional(inner) => inner,
            other => other,
        }
    }

    pub fn wrap_optional(self) -> QueryType {
        match self {
            QueryType::Optional(_) => self,
            QueryType::Tuple(elems) => {
                QueryType::Tuple(elems.into_iter().map(QueryType::wrap_optional).collect())
            }
            other => QueryType::Optional(Box::new(other)),
        }
    }

    pub fn is_unknown(&self) -> bool {
        match self {
            QueryType::Scalar(_) | QueryType::Void => false,
            QueryType::Optional(inner) | QueryType::List(inner) => inner.is_unknown(),
            QueryType::Tuple(elements) => elements.iter().any(QueryType::is_unknown),
            QueryType::Record(source) => match source {
                RecordSource::Schema(_) => false,
                RecordSource::Projection(fields) => fields.iter().any(|f| f.ty.is_unknown()),
            },
            QueryType::Unknown => true,
        }
    }
}

impl From<&ValueType> for QueryType {
    fn from(value: &ValueType) -> Self {
        match value {
            ValueType::Scalar(s) => QueryType::Scalar(*s),
            ValueType::Tuple(types) => {
                QueryType::Tuple(types.iter().map(QueryType::from).collect())
            }
            ValueType::Optional(inner) => QueryType::from(inner.as_ref()).wrap_optional(),
            ValueType::Array { element, .. } => {
                QueryType::List(Box::new(QueryType::from(element.as_ref())))
            }
        }
    }
}

impl From<TypeName> for QueryType {
    fn from(value: TypeName) -> Self {
        match value {
            TypeName::Int => QueryType::Scalar(ScalarType::Int),
            TypeName::Float => QueryType::Scalar(ScalarType::Float),
            TypeName::Dec => QueryType::Scalar(ScalarType::Dec),
        }
    }
}

impl fmt::Display for QueryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueryType::Scalar(s) => match s {
                ScalarType::Int => write!(f, "Int"),
                ScalarType::Float => write!(f, "Float"),
                ScalarType::Dec => write!(f, "Dec"),
                ScalarType::String => write!(f, "String"),
                ScalarType::Bool => write!(f, "Bool"),
                ScalarType::Instant => write!(f, "Instant"),
                ScalarType::Duration => write!(f, "Duration"),
            },
            QueryType::Optional(inner) => write!(f, "?{inner}"),
            QueryType::List(inner) => write!(f, "List<{inner}>"),
            QueryType::Tuple(elems) => {
                write!(f, "(")?;
                for (i, elem) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{elem}")?;
                }
                write!(f, ")")
            }
            QueryType::Record(RecordSource::Projection(fields)) => {
                write!(f, "{{ ")?;
                for (i, field) in fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{}: {}", field.name, field.ty)?;
                }
                write!(f, " }}")
            }
            QueryType::Record(RecordSource::Schema(struct_id)) => {
                write!(f, "Record#{}", struct_id.index())
            }
            QueryType::Void => write!(f, "Void"),
            QueryType::Unknown => write!(f, "_"),
        }
    }
}
