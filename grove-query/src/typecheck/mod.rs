pub mod error;
pub mod types;

use std::collections::HashMap;

use crate::ast::{Arg, BinaryOp, Expr, Literal, QueryFile, Statement, UnaryOp};
use crate::typecheck::error::TypeError;
use crate::typecheck::types::*;
use grove_schema::validated::{Field, ScalarType, StructId, ValidatedSchema};
use grove_types::{Diagnostic, Span, Spanned};

struct TypeEnv<'s> {
    scopes: Vec<HashMap<String, QueryType>>,
    schema: &'s ValidatedSchema,
}

impl<'s> TypeEnv<'s> {
    fn new(schema: &'s ValidatedSchema) -> Self {
        let mut env = TypeEnv {
            scopes: vec![HashMap::new()],
            schema,
        };

        for root in &schema.roots {
            let record_ty = QueryType::Record(RecordSource::Schema(root.struct_id));
            let list_ty = QueryType::List(Box::new(record_ty));
            env.define(root.name.clone(), list_ty);
        }

        env
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn define(&mut self, name: String, ty: QueryType) {
        self.scopes.last_mut().unwrap().insert(name, ty);
    }

    fn resolve(&self, name: &str) -> Option<&QueryType> {
        for scope in self.scopes.iter().rev() {
            if let Some(ty) = scope.get(name) {
                return Some(ty);
            }
        }
        None
    }

    fn schema(&self) -> &ValidatedSchema {
        self.schema
    }

    fn push_record_fields(&mut self, ty: &QueryType) -> bool {
        match ty {
            QueryType::Record(RecordSource::Schema(struct_id)) => {
                self.push_scope();
                self.push_schema_struct_fields(*struct_id);
                true
            }
            QueryType::Record(RecordSource::Projection(fields)) => {
                self.push_scope();
                for field in fields {
                    self.define(field.name.clone(), field.ty.clone());
                }
                true
            }
            QueryType::List(inner) => self.push_record_fields(inner),
            _ => false,
        }
    }

    fn push_schema_struct_fields(&mut self, struct_id: StructId) {
        let struct_ = &self.schema.structs[struct_id.index()];
        for field in &struct_.fields {
            let (name, ty) = match field {
                Field::Value { name, ty, .. } => (name.clone(), ty.into()),
                Field::Array { name, element, .. } => {
                    (name.clone(), QueryType::List(Box::new(element.into())))
                }
                Field::Ref {
                    name,
                    target,
                    optional,
                    is_list,
                    ..
                } => {
                    let inner = QueryType::Record(RecordSource::Schema(*target));
                    let ty = if *is_list {
                        QueryType::List(Box::new(inner))
                    } else if *optional {
                        inner.wrap_optional()
                    } else {
                        inner
                    };
                    (name.clone(), ty)
                }
            };
            self.define(name, ty);
        }
    }
}

fn field_query_type(field: &Field) -> QueryType {
    match field {
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

fn record_field_by_name(
    record: &QueryType,
    name: &str,
    schema: &ValidatedSchema,
) -> Option<QueryType> {
    match record {
        QueryType::Record(RecordSource::Schema(struct_id)) => {
            schema.struct_field(*struct_id, name).map(field_query_type)
        }
        QueryType::Record(RecordSource::Projection(fields)) => {
            fields.iter().find(|f| f.name == name).map(|f| f.ty.clone())
        }
        _ => None,
    }
}

fn infer_literal(literal: &Literal) -> QueryType {
    match literal {
        Literal::Int(_) => QueryType::Scalar(ScalarType::Int),
        Literal::Float(_) => QueryType::Scalar(ScalarType::Float),
        Literal::Dec(_) => QueryType::Scalar(ScalarType::Dec),
        Literal::String(_) => QueryType::Scalar(ScalarType::String),
        Literal::Bool(_) => QueryType::Scalar(ScalarType::Bool),
        Literal::Instant(_) | Literal::Now | Literal::Today(_) => {
            QueryType::Scalar(ScalarType::Instant)
        }
        Literal::Duration(_) => QueryType::Scalar(ScalarType::Duration),
        Literal::None => QueryType::Unknown.wrap_optional(),
    }
}

fn infer(expr: &Expr, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(lit) => {
            let ty = infer_literal(&lit.value);
            Ok(TypedExpr {
                kind: TypedExprKind::Literal(lit.clone()),
                ty,
                span: lit.span,
            })
        }
        Expr::Ident(name) => {
            let ty =
                env.resolve(&name.value)
                    .cloned()
                    .ok_or_else(|| TypeError::UnknownIdentifier {
                        name: name.value.clone(),
                        span: name.span,
                    })?;
            Ok(TypedExpr {
                kind: TypedExprKind::Ident(name.clone()),
                ty,
                span: name.span,
            })
        }
        Expr::Field {
            base,
            name,
            optional,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_field(typed_base, name, *optional, env.schema())
        }
        Expr::Method {
            base,
            name,
            args,
            optional,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_method(typed_base, name, args, *optional, env)
        }
        Expr::Binary { op, lhs, rhs, .. } => infer_binary(op, lhs, rhs, env),
        Expr::Unary { op, expr, .. } => infer_unary(op, expr, env),
        Expr::Tuple { elements, .. } => {
            let mut typed_elems = Vec::new();
            for elem in elements {
                typed_elems.push(infer(elem, env)?);
            }
            let ty = QueryType::Tuple(typed_elems.iter().map(|e| e.ty.clone()).collect());
            Ok(TypedExpr {
                kind: TypedExprKind::Tuple {
                    elements: typed_elems,
                },
                ty,
                span: expr.span(),
            })
        }
        _ => Err(TypeError::AmbiguousType { span: expr.span() }),
    }
}

fn infer_field(
    base: TypedExpr,
    name: &Spanned<String>,
    optional: bool,
    schema: &ValidatedSchema,
) -> Result<TypedExpr, TypeError> {
    let lookup_ty = if optional {
        match &base.ty {
            QueryType::Optional(inner) => inner.as_ref().clone(),
            other => {
                return Err(TypeError::ArgTypeMismatch {
                    method: name.value.clone(),
                    expected: "optional type".into(),
                    got: other.to_string(),
                    span: name.span,
                });
            }
        }
    } else {
        base.ty.clone()
    };

    let (inner, was_list) = match &lookup_ty {
        QueryType::List(inner) => (inner.as_ref().clone(), true),
        other => (other.clone(), false),
    };

    let mut field_ty = record_field_by_name(&inner, &name.value, schema).ok_or_else(|| {
        TypeError::UnknownField {
            field: name.value.clone(),
            base_ty: base.ty.to_string(),
            span: name.span,
        }
    })?;

    if was_list {
        field_ty = QueryType::List(Box::new(field_ty));
    }
    if optional {
        field_ty = field_ty.wrap_optional();
    }

    let span = Span::new(base.span.start, name.span.end);
    Ok(TypedExpr {
        kind: TypedExprKind::Field {
            base: Box::new(base),
            name: name.clone(),
            optional,
        },
        ty: field_ty,
        span,
    })
}

fn infer_method(
    base: TypedExpr,
    name: &Spanned<String>,
    args: &[Arg],
    optional: bool,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let resolve_ty = if optional {
        match &base.ty {
            QueryType::Optional(inner) => inner.as_ref().clone(),
            other => {
                return Err(TypeError::ArgTypeMismatch {
                    method: name.value.clone(),
                    expected: "optional type".into(),
                    got: other.to_string(),
                    span: name.span,
                });
            }
        }
    } else {
        base.ty.clone()
    };

    let mut signature =
        method_signature(&resolve_ty, &name.value).ok_or_else(|| TypeError::UnknownMethod {
            method: name.value.clone(),
            base_ty: base.ty.to_string(),
            span: name.span,
        })?;

    match &signature.args {
        ArgCheck::Fixed(expected) => {
            if args.len() != expected.len() {
                return Err(TypeError::WrongArgCount {
                    method: name.value.clone(),
                    expected: expected.len(),
                    got: args.len(),
                    span: name.span,
                });
            }
        }
        ArgCheck::Scoped { count, .. } => match count {
            ArgCount::Exact(n) => {
                if args.len() != *n {
                    return Err(TypeError::WrongArgCount {
                        method: name.value.clone(),
                        expected: *n,
                        got: args.len(),
                        span: name.span,
                    });
                }
            }
            ArgCount::AtLeast(min) => {
                if args.len() < *min {
                    return Err(TypeError::WrongArgCount {
                        method: name.value.clone(),
                        expected: *min,
                        got: args.len(),
                        span: name.span,
                    });
                }
            }
        },
    }

    let is_scoped = matches!(&signature.args, ArgCheck::Scoped { .. });
    let pushed_scope = is_scoped && env.push_record_fields(&resolve_ty);

    let mut typed_args = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        let mut typed = infer(&arg.expr, env)?;
        match &mut signature.args {
            ArgCheck::Fixed(expected) => {
                if let Some(exp_ty) = expected.get_mut(i)
                    && !types_compatible(&mut typed.ty, exp_ty, env.schema())
                {
                    return Err(TypeError::ArgTypeMismatch {
                        method: name.value.clone(),
                        expected: exp_ty.to_string(),
                        got: typed.ty.to_string(),
                        span: arg.expr.span(),
                    });
                }
            }
            ArgCheck::Scoped { constraint, .. } => {
                if !constraint(&typed.ty) {
                    return Err(TypeError::ArgTypeMismatch {
                        method: name.value.clone(),
                        expected: "valid argument type".into(),
                        got: typed.ty.to_string(),
                        span: arg.expr.span(),
                    });
                }
            }
        }
        typed_args.push(typed);
    }

    if pushed_scope {
        env.pop_scope();
    }

    let return_ty = match signature.return_type {
        QueryType::Unknown if !typed_args.is_empty() => typed_args[0].ty.clone().wrap_optional(),
        _ => signature.return_type,
    };

    let return_ty = if optional {
        return_ty.wrap_optional()
    } else {
        return_ty
    };

    let span = Span::new(base.span.start, name.span.end);
    Ok(TypedExpr {
        kind: TypedExprKind::Method {
            base: Box::new(base),
            name: name.clone(),
            args: typed_args,
            optional,
        },
        ty: return_ty,
        span,
    })
}

fn infer_binary(
    op: &Spanned<BinaryOp>,
    lhs: &Expr,
    rhs: &Expr,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_lhs = infer(lhs, env)?;
    let mut typed_rhs = infer(rhs, env)?;
    let span = Span::new(typed_lhs.span.start, typed_rhs.span.end);

    let result_ty = match op.value {
        BinaryOp::In => infer_binary_in(&mut typed_lhs.ty, &mut typed_rhs.ty, span, env.schema())?,
        BinaryOp::And | BinaryOp::Or => {
            if !typed_lhs.ty.is_bool() || !typed_rhs.ty.is_bool() {
                return Err(TypeError::BinaryOpTypeMismatch {
                    left: typed_lhs.ty.to_string(),
                    op: op.value.to_string(),
                    right: typed_rhs.ty.to_string(),
                    span,
                });
            }
            QueryType::Scalar(ScalarType::Bool)
        }
        BinaryOp::Eq | BinaryOp::Ne => infer_equality_op(
            &mut typed_lhs.ty,
            &op.value,
            &mut typed_rhs.ty,
            span,
            env.schema(),
        )?,
        BinaryOp::Lt | BinaryOp::Gt | BinaryOp::Le | BinaryOp::Ge => infer_comparison_op(
            &mut typed_lhs.ty,
            &op.value,
            &mut typed_rhs.ty,
            span,
            env.schema(),
        )?,
        _ => infer_arithmetic_op(&mut typed_lhs.ty, &op.value, &mut typed_rhs.ty, span)?,
    };

    Ok(TypedExpr {
        kind: TypedExprKind::Binary {
            op: op.clone(),
            lhs: Box::new(typed_lhs),
            rhs: Box::new(typed_rhs),
        },
        ty: result_ty,
        span,
    })
}

fn infer_binary_in(
    lhs: &mut QueryType,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    let rhs_inner = match rhs {
        QueryType::Tuple(elems) if !elems.is_empty() => elems,
        _ => return Err(TypeError::InRequiresTuple { span }),
    };
    for elem in rhs_inner {
        if !types_compatible(lhs, elem, schema) {
            return Err(TypeError::BinaryOpTypeMismatch {
                left: lhs.to_string(),
                op: "in".into(),
                right: rhs.to_string(),
                span,
            });
        }
    }
    Ok(QueryType::Scalar(ScalarType::Bool))
}

fn infer_equality_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    if !types_compatible(lhs, rhs, schema) {
        return Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        });
    }
    Ok(QueryType::Scalar(ScalarType::Bool))
}

fn infer_comparison_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
    schema: &ValidatedSchema,
) -> Result<QueryType, TypeError> {
    if !types_compatible(lhs, rhs, schema) || !lhs.has_defined_order() {
        return Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        });
    }
    Ok(QueryType::Scalar(ScalarType::Bool))
}

fn infer_arithmetic_op(
    lhs: &mut QueryType,
    op: &BinaryOp,
    rhs: &mut QueryType,
    span: Span,
) -> Result<QueryType, TypeError> {
    use ScalarType::*;

    match (lhs, rhs) {
        (QueryType::Scalar(Int), QueryType::Scalar(Int)) => Ok(QueryType::Scalar(Int)),
        (QueryType::Scalar(Float), QueryType::Scalar(Float)) => Ok(QueryType::Scalar(Float)),
        (QueryType::Scalar(Dec), QueryType::Scalar(Dec)) => Ok(QueryType::Scalar(Dec)),
        (lhs @ QueryType::Scalar(Instant), rhs @ QueryType::Scalar(Duration)) => {
            if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                Ok(QueryType::Scalar(Instant))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Instant), rhs @ QueryType::Scalar(Instant)) => {
            if matches!(op, BinaryOp::Sub) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Duration), rhs @ QueryType::Scalar(Duration)) => {
            if matches!(op, BinaryOp::Add | BinaryOp::Sub) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs, rhs @ QueryType::Scalar(Duration)) if lhs.is_numeric() => {
            if matches!(op, BinaryOp::Mul) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs @ QueryType::Scalar(Duration), rhs) if rhs.is_numeric() => {
            if matches!(
                op,
                BinaryOp::Mul | BinaryOp::Div | BinaryOp::Rem | BinaryOp::Mod
            ) {
                Ok(QueryType::Scalar(Duration))
            } else {
                Err(TypeError::BinaryOpTypeMismatch {
                    left: lhs.to_string(),
                    op: op.to_string(),
                    right: rhs.to_string(),
                    span,
                })
            }
        }
        (lhs, rhs) => Err(TypeError::BinaryOpTypeMismatch {
            left: lhs.to_string(),
            op: op.to_string(),
            right: rhs.to_string(),
            span,
        }),
    }
}

fn infer_unary(
    op: &Spanned<UnaryOp>,
    expr: &Expr,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let typed_expr = infer(expr, env)?;
    let span = Span::new(op.span.start, typed_expr.span.end);

    let result_ty = match op.value {
        UnaryOp::Neg => match &typed_expr.ty {
            QueryType::Scalar(ScalarType::Int) => QueryType::Scalar(ScalarType::Int),
            QueryType::Scalar(ScalarType::Float) => QueryType::Scalar(ScalarType::Float),
            QueryType::Scalar(ScalarType::Dec) => QueryType::Scalar(ScalarType::Dec),
            QueryType::Scalar(ScalarType::Duration) => QueryType::Scalar(ScalarType::Duration),
            _ => {
                return Err(TypeError::UnaryOpTypeMismatch {
                    op: op.to_string(),
                    operand: typed_expr.ty.to_string(),
                    span,
                });
            }
        },
        UnaryOp::Not => {
            if !typed_expr.ty.is_bool() {
                return Err(TypeError::UnaryOpTypeMismatch {
                    op: op.to_string(),
                    operand: typed_expr.ty.to_string(),
                    span,
                });
            }
            QueryType::Scalar(ScalarType::Bool)
        }
    };

    Ok(TypedExpr {
        kind: TypedExprKind::Unary {
            op: op.clone(),
            expr: Box::new(typed_expr),
        },
        ty: result_ty,
        span,
    })
}

fn types_compatible(a: &mut QueryType, b: &mut QueryType, schema: &ValidatedSchema) -> bool {
    match (a, b) {
        (QueryType::Unknown, QueryType::Unknown) => true,
        (a @ QueryType::Unknown, b @ QueryType::Scalar(_)) => {
            *a = b.clone();
            true
        }
        (a @ QueryType::Scalar(_), b @ QueryType::Unknown) => {
            *b = a.clone();
            true
        }
        (QueryType::Optional(a), QueryType::Optional(b)) => types_compatible(a, b, schema),
        (QueryType::List(a), QueryType::List(b)) => types_compatible(a, b, schema),
        (QueryType::Scalar(a), QueryType::Scalar(b)) => a == b,
        (QueryType::Tuple(a), QueryType::Tuple(b)) => {
            a.len() == b.len()
                && a.iter_mut()
                    .zip(b.iter_mut())
                    .all(|(a, b)| types_compatible(a, b, schema))
        }
        (QueryType::Record(a), QueryType::Record(b)) => record_fields_match(a, b, schema),
        (a, b) => a == b,
    }
}

fn record_source_field_count(source: &RecordSource, schema: &ValidatedSchema) -> usize {
    match source {
        RecordSource::Schema(id) => schema.structs[id.index()].fields.len(),
        RecordSource::Projection(fields) => fields.len(),
    }
}

fn record_source_field_type<'a>(
    source: &'a RecordSource,
    name: &str,
    schema: &'a ValidatedSchema,
) -> Option<QueryType> {
    match source {
        RecordSource::Schema(id) => schema.struct_field(*id, name).map(field_query_type),
        RecordSource::Projection(fields) => {
            fields.iter().find(|f| f.name == name).map(|f| f.ty.clone())
        }
    }
}

fn record_fields_match(a: &RecordSource, b: &RecordSource, schema: &ValidatedSchema) -> bool {
    if a == b {
        return true;
    }
    if record_source_field_count(a, schema) != record_source_field_count(b, schema) {
        return false;
    }

    let a_names: Vec<&str> = match a {
        RecordSource::Schema(id) => schema.structs[id.index()]
            .fields
            .iter()
            .map(|f| match f {
                Field::Value { name, .. } | Field::Array { name, .. } | Field::Ref { name, .. } => {
                    name.as_str()
                }
            })
            .collect(),
        RecordSource::Projection(fields) => fields.iter().map(|f| f.name.as_str()).collect(),
    };

    a_names.iter().all(|name| {
        let a_ty = record_source_field_type(a, name, schema);
        let b_ty = record_source_field_type(b, name, schema);
        match (a_ty, b_ty) {
            (Some(mut a_ty), Some(mut b_ty)) => types_compatible(&mut a_ty, &mut b_ty, schema),
            _ => false,
        }
    })
}

#[derive(Debug, Clone, Copy)]
enum ArgCount {
    Exact(usize),
    AtLeast(usize),
}

#[derive(Debug, Clone)]
enum ArgCheck {
    Fixed(Vec<QueryType>),
    Scoped {
        count: ArgCount,
        constraint: fn(&QueryType) -> bool,
    },
}

#[derive(Debug, Clone)]
struct MethodSig {
    return_type: QueryType,
    args: ArgCheck,
}

impl MethodSig {
    fn fixed(return_type: QueryType, params: Vec<QueryType>) -> Self {
        MethodSig {
            return_type,
            args: ArgCheck::Fixed(params),
        }
    }

    fn no_args(return_type: QueryType) -> Self {
        Self::fixed(return_type, vec![])
    }

    fn one_arg(return_type: QueryType, param: QueryType) -> Self {
        Self::fixed(return_type, vec![param])
    }

    fn scoped_fixed(return_type: QueryType, n: usize, constraint: fn(&QueryType) -> bool) -> Self {
        MethodSig {
            return_type,
            args: ArgCheck::Scoped {
                count: ArgCount::Exact(n),
                constraint,
            },
        }
    }

    fn scoped_at_least(
        return_type: QueryType,
        min: usize,
        constraint: fn(&QueryType) -> bool,
    ) -> Self {
        MethodSig {
            return_type,
            args: ArgCheck::Scoped {
                count: ArgCount::AtLeast(min),
                constraint,
            },
        }
    }
}

fn method_signature(base: &QueryType, method: &str) -> Option<MethodSig> {
    use ScalarType::*;

    match base {
        QueryType::Scalar(s) => scalar_method(s, method),
        QueryType::Optional(inner) => match method {
            "unwrap" => Some(MethodSig::no_args(inner.as_ref().clone())),
            "unwrap_or" => Some(MethodSig::one_arg(
                inner.as_ref().clone(),
                inner.as_ref().clone(),
            )),
            "is_none" | "is_some" => Some(MethodSig::no_args(QueryType::Scalar(Bool))),
            _ => None,
        },
        QueryType::List(inner) => match method {
            "len" => Some(MethodSig::no_args(QueryType::Scalar(Int))),
            "first" => Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional())),
            "nth" => Some(MethodSig::one_arg(
                inner.as_ref().clone().wrap_optional(),
                QueryType::Scalar(Int),
            )),
            "contains" => Some(MethodSig::one_arg(
                QueryType::Scalar(Bool),
                inner.as_ref().clone(),
            )),
            "sum" if inner.is_summable() => {
                Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional()))
            }
            "avg" if inner.is_summable() => {
                Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional()))
            }
            "max" | "min" if inner.has_defined_order() => {
                Some(MethodSig::no_args(inner.as_ref().clone().wrap_optional()))
            }
            "sort" | "sort_asc" | "sort_desc" => Some(MethodSig::scoped_at_least(
                inner.as_ref().clone(),
                1,
                QueryType::has_defined_order,
            )),
            "sum_over" | "avg_over" if matches!(inner.as_ref(), QueryType::Record(_)) => {
                Some(MethodSig::scoped_fixed(
                    QueryType::Unknown.wrap_optional(),
                    1,
                    QueryType::is_summable,
                ))
            }
            "max_by" | "min_by" if matches!(inner.as_ref(), QueryType::Record(_)) => {
                Some(MethodSig::scoped_fixed(
                    inner.as_ref().clone().wrap_optional(),
                    1,
                    QueryType::has_defined_order,
                ))
            }
            _ => None,
        },
        _ => None,
    }
}

fn scalar_method(s: &ScalarType, method: &str) -> Option<MethodSig> {
    use ScalarType::*;
    match s {
        String => match method {
            "contains" | "starts_with" | "ends_with" => Some(MethodSig::one_arg(
                QueryType::Scalar(Bool),
                QueryType::Scalar(String),
            )),
            "to_upper" | "to_lower" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            "len" => Some(MethodSig::no_args(QueryType::Scalar(Int))),
            _ => None,
        },
        Instant => match method {
            "year" | "month" | "day" | "hour" | "minute" | "weekday" => {
                Some(MethodSig::no_args(QueryType::Scalar(Int)))
            }
            "second" | "epoch" => Some(MethodSig::no_args(QueryType::Scalar(Dec))),
            "to_rfc3339" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
        Duration => match method {
            "as_seconds" | "as_minutes" | "as_hours" | "as_days" => {
                Some(MethodSig::no_args(QueryType::Scalar(Dec)))
            }
            _ => None,
        },
        Dec => match method {
            "round_dp" => Some(MethodSig::one_arg(
                QueryType::Scalar(Dec),
                QueryType::Scalar(Int),
            )),
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
        Int => match method {
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
        Float => match method {
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            "round" | "floor" | "ceil" => Some(MethodSig::no_args(QueryType::Scalar(Float))),
            _ => None,
        },
        Bool => match method {
            "to_string" => Some(MethodSig::no_args(QueryType::Scalar(String))),
            _ => None,
        },
    }
}

#[allow(dead_code)] // not yet used
fn check(expr: &Expr, expected: &QueryType, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(Spanned {
            value: Literal::None,
            span,
        }) => {
            let ty = expected.clone().wrap_optional();
            Ok(TypedExpr {
                kind: TypedExprKind::Literal(Spanned {
                    span: *span,
                    value: Literal::None,
                }),
                ty,
                span: *span,
            })
        }
        _ => infer(expr, env),
    }
}

fn typecheck_stmt(stmt: &Statement, env: &mut TypeEnv) -> Result<TypedStatement, TypeError> {
    match stmt {
        Statement::Expr(expr) => {
            let typed = infer(expr, env)?;
            Ok(TypedStatement::Expr(typed))
        }
        Statement::Mutation(_) => todo!(),
    }
}

pub fn typecheck(
    file: QueryFile,
    schema: &ValidatedSchema,
) -> (Option<TypedQueryFile>, Vec<Diagnostic>) {
    let mut env = TypeEnv::new(schema);
    let mut diags: Vec<Diagnostic> = Vec::new();
    let mut statements = Vec::new();

    for stmt in &file.statements {
        match typecheck_stmt(stmt, &mut env) {
            Ok(typed) => statements.push(typed),
            Err(err) => diags.push(err.into()),
        }
    }

    match infer(&file.result, &mut env) {
        Ok(result) => (Some(TypedQueryFile { statements, result }), diags),
        Err(err) => {
            diags.push(err.into());
            (None, diags)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_SCHEMA_SRC: &str = r#"
        config {
            int_arithmetic = "checked",
            float_checks = true,
            dec_arithmetic = "checked",
        }
        root users: User;
        struct User {
            name: String,
            age: Int,
            score: Float,
            balance: Dec,
            active: Bool,
            created: Instant,
            profile: ?Profile,
        }
        struct Profile {
            user: &User,
            username: String
        }
        rel Profile.user <-> User.profile (profiles.user_id -> users.id);
    "#;

    fn test_schema() -> ValidatedSchema {
        grove_schema::validate(
            grove_schema::parse_schema(TEST_SCHEMA_SRC)
                .0
                .expect("invalid test schema"),
        )
        .0
        .expect("invalid test schema")
    }

    #[test]
    fn infer_int_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("42");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn infer_float_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("3.14f");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn infer_dec_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("3.14");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn infer_string_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("\"hello\"");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn infer_bool_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("true");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn infer_instant_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("@now");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Instant));
    }

    #[test]
    fn infer_duration_literal() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("#30d");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn none_inferred_type() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let expected = QueryType::Scalar(ScalarType::Int).wrap_optional();
        let (file, _diags) = crate::parse_query("none");
        let result = check(&file.unwrap().result, &expected, &mut env).unwrap();
        assert_eq!(result.ty, expected);
    }

    #[test]
    fn none_without_context_unknown() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("none");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Unknown.wrap_optional());
    }

    #[test]
    fn root_lookup() {
        let schema = test_schema();
        let env = TypeEnv::new(&schema);
        let ty = env.resolve("users").unwrap();
        assert!(matches!(ty, QueryType::List(_)));
    }

    #[test]
    fn root_unknown_identifier() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("posts");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn field_access_struct() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn field_access_unknown_field() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.unknown");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_field_access() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.profile");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert!(
            matches!(result.ty, QueryType::List(inner) if matches!(*inner, QueryType::Optional(_)))
        );
    }

    #[test]
    fn scalar_method() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query(r#""xyz".starts_with("abc")"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn list_method_len() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.len()");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn list_method_nth() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.nth(0)");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert!(matches!(result.ty, QueryType::Optional(inner)
            if matches!(*inner, QueryType::Record(_))
        ));
    }

    #[test]
    fn method_unwrap_access() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.first().unwrap().name");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn optional_method_is_some() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.first().is_some()");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn method_unknown() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name.nonexistent()");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn method_wrong_arg_count() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.name.contains(\"a\", \"b\")");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn method_arg_constraint_valid() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.sum_over(age)");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_ok());
    }

    #[test]
    fn method_arg_constraint_invalid() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _diags) = crate::parse_query("users.sort_asc(profile)");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_int_add() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 + 2");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_int_sub() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("5 - 3");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_int_mul() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("4 * 3");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_int_div() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("10 / 2");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_int_rem() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("10 % 3");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_int_mod() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("10 mod 3");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_float_add() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0f + 2.0f");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn binary_dec_add() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0 + 2.0");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn binary_int_float_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 + 2.0f");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_float_dec_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0f + 2.0");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_add() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d + #5h");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_duration_sub() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d - #5h");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_duration_mul_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d * 3");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_int_mul_duration() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("3 * #30d");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_duration_mul_float() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d * 1.5f");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_duration_mul_dec() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d * 1.5");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_duration_div_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d / 2");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_duration_rem_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d % 7");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn binary_duration_add_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d + 3");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_duration_mul_duration_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d * #5h");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_comparison_eq() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 == 2");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_comparison_ne() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0f != 2e5f");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_comparison_lt() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 < 2");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_comparison_gt() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("10.5 > 11.0");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_comparison_le() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("20 <= 40");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_comparison_ge() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1001 >= 1000");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_string_eq() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#""a" == "b""#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_string_lt() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#""x" < "y""#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_comparison_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"1 == "a""#);
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_logical_and() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("true && false");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_logical_or() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("false || true");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_logical_and_non_bool_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 && 2");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_in_string_tuple() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#""a" in ("a", "b", "c")"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_in_non_tuple_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 in 2");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_in_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"1 in ("a", "b")"#);
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_eq_optional() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) =
            crate::parse_query("users.first().unwrap().profile == users.first().unwrap().profile");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn optional_eq_non_optional_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("none != 1");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn optional_ordering_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("none < none");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_neg_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("-1");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn unary_neg_float() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("-3e6f");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn unary_neg_dec() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("-12.5");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn unary_neg_duration() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("-#30d");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }

    #[test]
    fn unary_neg_instant_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("-@now");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_neg_string_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"-"hello""#);
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn unary_not_bool() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("!true");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn unary_not_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("!1");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn binary_complex_arithmetic() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1 + 2 * (3 / 2) % (7 - 2)");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_arithmetic_comparison() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("(1 + 2 - 3) >= 3");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_field_arithmetic() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users.first().unwrap().age + 1");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn binary_field_comparison() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users.first().unwrap().age < 18");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn binary_duration_arithmetic() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("#30d + #24h - #7d + #7.8s");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Duration));
    }
}
