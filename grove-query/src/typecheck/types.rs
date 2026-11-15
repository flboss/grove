use std::fmt;

use crate::ast::{BinaryOp, ConstantName, Literal, SortDir, TypeName, UnaryOp};
use grove_schema::validated::{
    EnumId, Field, FieldId, ScalarType, StructId, ValidatedSchema, ValueType,
};
use grove_types::{NodeId, Span, Spanned};

#[derive(Debug, Clone, PartialEq)]
pub struct TypedQueryFile {
    /// Side-effect statements and discarded expression statements
    pub statements: Vec<TypedStatement>,
    /// Final expression statement, if present
    pub result: Option<TypedExpr>,
}

impl TypedQueryFile {
    pub fn output_type(&self) -> Option<&QueryType> {
        self.result.as_ref().map(|expr| &expr.ty)
    }

    pub fn let_bindings(&self) -> impl Iterator<Item = (&str, &QueryType)> {
        self.statements.iter().filter_map(|stmt| match stmt {
            TypedStatement::Let(let_stmt) => {
                Some((let_stmt.name.value.as_str(), &let_stmt.value.ty))
            }
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedStatement {
    Mutation(TypedMutationStmt),
    Let(TypedLetStmt),
    Expr(TypedExpr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypedLetStmt {
    pub name: Spanned<String>,
    pub value: TypedExpr,
    pub span: Span,
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
    pub id: NodeId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypedExprKind {
    Literal(Spanned<Literal>),
    Ident {
        name: Spanned<String>,
        binding: IdentBinding,
        depth: usize,
    },
    EnumVariant {
        enum_id: EnumId,
        variant: usize,
    },
    Field {
        base: Box<TypedExpr>,
        name: Spanned<String>,
        opt_depth: usize,
    },
    TupleAccess {
        base: Box<TypedExpr>,
        index: usize,
        opt_depth: usize,
    },
    Method {
        base: Box<TypedExpr>,
        name: Spanned<String>,
        args: Vec<TypedMethodArg>,
        opt_depth: usize,
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
    ListElement,
    Let,
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

impl RecordSource {
    fn eq(&self, other: &RecordSource, schema: &ValidatedSchema) -> bool {
        match (self, other) {
            (RecordSource::Schema(left), RecordSource::Schema(right)) => left == right,
            (RecordSource::Projection(left), RecordSource::Projection(right)) => {
                left.len() == right.len()
                    && left.iter().all(|left_field| {
                        right.iter().any(|right_field| {
                            left_field.name == right_field.name
                                && left_field.ty.eq(&right_field.ty, schema)
                        })
                    })
            }
            (RecordSource::Schema(struct_id), RecordSource::Projection(fields))
            | (RecordSource::Projection(fields), RecordSource::Schema(struct_id)) => {
                let struct_ = &schema.structs[struct_id.index()];
                struct_.fields.len() == fields.len()
                    && struct_.fields.iter().all(|schema_field| {
                        fields.iter().any(|field| {
                            field.name == schema_field.name()
                                && QueryType::from(schema_field).eq(&field.ty, schema)
                        })
                    })
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum QueryType {
    Scalar(ScalarType),
    Enum(EnumId),
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
        QueryType::Optional(Box::new(self))
    }

    pub fn unwrap_optional_depth(&self, depth: usize) -> Option<&QueryType> {
        let mut ty = self;
        for _ in 0..depth {
            let QueryType::Optional(inner) = ty else {
                return None;
            };
            ty = inner;
        }
        Some(ty)
    }

    pub fn wrap_optional_depth(self, depth: usize) -> QueryType {
        let mut ty = self;
        for _ in 0..depth {
            ty = ty.wrap_optional();
        }
        ty
    }

    pub fn is_unknown(&self) -> bool {
        match self {
            QueryType::Scalar(_) | QueryType::Enum(_) | QueryType::Void => false,
            QueryType::Optional(inner) | QueryType::List(inner) => inner.is_unknown(),
            QueryType::Tuple(elements) => elements.iter().any(QueryType::is_unknown),
            QueryType::Record(source) => match source {
                RecordSource::Schema(_) => false,
                RecordSource::Projection(fields) => fields.iter().any(|f| f.ty.is_unknown()),
            },
            QueryType::Unknown => true,
        }
    }

    pub fn is_scalar(&self) -> bool {
        match self {
            QueryType::Scalar(_) => true,
            QueryType::Optional(inner) => matches!(inner.as_ref(), QueryType::Scalar(_)),
            _ => false,
        }
    }

    pub fn schema_record(&self) -> Option<StructId> {
        match self {
            QueryType::Record(RecordSource::Schema(struct_id)) => Some(*struct_id),
            QueryType::Optional(inner) | QueryType::List(inner) => inner.schema_record(),
            _ => None,
        }
    }

    pub fn single_schema_record(&self) -> Option<StructId> {
        match self {
            QueryType::Record(RecordSource::Schema(struct_id)) => Some(*struct_id),
            QueryType::Optional(inner) => inner.single_schema_record(),
            _ => None,
        }
    }

    pub fn eq(&self, other: &QueryType, schema: &ValidatedSchema) -> bool {
        match (self, other) {
            (QueryType::Scalar(left), QueryType::Scalar(right)) => left == right,
            (QueryType::Enum(left), QueryType::Enum(right)) => left == right,
            (QueryType::Optional(left), QueryType::Optional(right)) => {
                left.as_ref().eq(right, schema)
            }
            (QueryType::List(left), QueryType::List(right)) => left.as_ref().eq(right, schema),
            (QueryType::Tuple(left), QueryType::Tuple(right)) => {
                left.len() == right.len()
                    && left
                        .iter()
                        .zip(right)
                        .all(|(left, right)| left.eq(right, schema))
            }
            (QueryType::Record(left), QueryType::Record(right)) => left.eq(right, schema),
            (QueryType::Void, QueryType::Void) | (QueryType::Unknown, QueryType::Unknown) => true,
            _ => false,
        }
    }
}

impl From<&Field> for QueryType {
    fn from(value: &Field) -> Self {
        match value {
            Field::Value { ty, .. } => ty.into(),
            Field::Array { element, .. } => QueryType::List(Box::new(element.into())),
            Field::Ref {
                target,
                optional,
                is_list,
                ..
            } => {
                let inner = QueryType::Record(RecordSource::Schema(*target));
                if *is_list {
                    QueryType::List(Box::new(inner))
                } else if *optional {
                    inner.wrap_optional()
                } else {
                    inner
                }
            }
        }
    }
}

impl From<&ValueType> for QueryType {
    fn from(value: &ValueType) -> Self {
        match value {
            ValueType::Scalar(s) => QueryType::Scalar(*s),
            ValueType::Enum(id) => QueryType::Enum(*id),
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
            TypeName::String => QueryType::Scalar(ScalarType::String),
            TypeName::Bytes => QueryType::Scalar(ScalarType::Bytes),
            TypeName::Bool => QueryType::Scalar(ScalarType::Bool),
            TypeName::Instant => QueryType::Scalar(ScalarType::Instant),
            TypeName::Duration => QueryType::Scalar(ScalarType::Duration),
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
                ScalarType::Bytes => write!(f, "Bytes"),
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
                write!(f, "Struct#{}", struct_id.index())
            }
            QueryType::Enum(enum_id) => write!(f, "Enum#{}", enum_id.index()),
            QueryType::Void => write!(f, "Void"),
            QueryType::Unknown => write!(f, "_"),
        }
    }
}
