pub mod error;
pub mod types;

use std::collections::{HashMap, HashSet};

use crate::ast::{
    Arg, BinaryOp, Expr, Literal, MutationKind, MutationStmt, ProjectionItem, QueryFile, Statement,
    TypeName, UnaryOp,
};
use crate::typecheck::error::TypeError;
use crate::typecheck::types::*;
use grove_schema::validated::{Field, ScalarType, StructId, ValidatedSchema, ValueType};
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
        Expr::Array { elements, span } => infer_array(elements, *span, env),
        Expr::Some { value, .. } => {
            let typed_value = infer(value, env)?;
            let ty = typed_value.ty.clone().wrap_optional();
            Ok(TypedExpr {
                kind: TypedExprKind::Some {
                    value: Box::new(typed_value),
                },
                ty,
                span: expr.span(),
            })
        }
        Expr::If {
            arms,
            default,
            span,
        } => infer_if(arms, default, *span, env),
        Expr::Cast { expr, ty, .. } => infer_cast(expr, ty, env),
        Expr::Projection { base, items, span } => infer_projection(base, items, *span, env),
        Expr::TypeConstant { ty, name } => infer_type_constant(ty, name),
        Expr::Struct { fields, span } => infer_struct_literal(fields, *span, env),
    }
}

fn infer_array(elements: &[Expr], span: Span, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    let mut typed_elems = Vec::with_capacity(elements.len());
    for elem in elements {
        typed_elems.push(infer(elem, env)?);
    }

    let mut ty = QueryType::Unknown;
    for elem in &mut typed_elems {
        if !types_compatible(&mut ty, &mut elem.ty, env.schema()) {
            return Err(TypeError::ArrayElementTypeMismatch {
                expected: ty.to_string(),
                got: elem.ty.to_string(),
                span: elem.span,
            });
        }
    }

    if !matches!(ty, QueryType::Unknown) {
        for elem in &mut typed_elems {
            types_compatible(&mut ty, &mut elem.ty, env.schema());
        }
    }

    let ty = QueryType::List(Box::new(ty));

    Ok(TypedExpr {
        kind: TypedExprKind::Array {
            elements: typed_elems,
        },
        ty,
        span,
    })
}

fn infer_if(
    arms: &[(Expr, Expr)],
    default: &Expr,
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_arms = Vec::with_capacity(arms.len());
    let mut typed_default = infer(default, env)?;

    for (cond, body) in arms {
        let typed_cond = infer(cond, env)?;
        if !typed_cond.ty.is_bool() {
            return Err(TypeError::IfConditionNotBool {
                span: typed_cond.span,
            });
        }
        let typed_body = infer(body, env)?;
        typed_arms.push((typed_cond, typed_body));
    }

    for (_, body) in &mut typed_arms {
        if !types_compatible(&mut typed_default.ty, &mut body.ty, env.schema()) {
            return Err(TypeError::IfBranchTypeMismatch {
                expected: typed_default.ty.to_string(),
                got: body.ty.to_string(),
                span: body.span,
            });
        }
    }

    for (_, body) in &mut typed_arms {
        types_compatible(&mut typed_default.ty, &mut body.ty, env.schema());
    }

    let ty = typed_default.ty.clone();

    Ok(TypedExpr {
        kind: TypedExprKind::If {
            arms: typed_arms,
            default: Box::new(typed_default),
        },
        ty,
        span,
    })
}

fn infer_cast(
    expr: &Expr,
    target: &Spanned<TypeName>,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let typed_expr = infer(expr, env)?;
    let span = Span::new(typed_expr.span.start, target.span.end);

    let target_ty = target.value.into();

    if !cast_allowed(&typed_expr.ty, &target_ty) {
        return Err(TypeError::InvalidCast {
            from: typed_expr.ty.to_string(),
            to: target_ty.to_string(),
            span,
        });
    }

    Ok(TypedExpr {
        kind: TypedExprKind::Cast {
            expr: Box::new(typed_expr),
            ty: target.clone(),
        },
        ty: target_ty,
        span,
    })
}

fn cast_allowed(from: &QueryType, to: &QueryType) -> bool {
    use ScalarType::*;
    from.is_numeric() && to.is_numeric()
        || matches!(
            (from, to),
            (QueryType::Scalar(Bool), QueryType::Scalar(Int))
        )
}

fn infer_type_constant(
    type_name: &Spanned<TypeName>,
    name: &Spanned<crate::ast::ConstantName>,
) -> Result<TypedExpr, TypeError> {
    let scalar_ty = match type_name.value {
        TypeName::Int => ScalarType::Int,
        TypeName::Float => ScalarType::Float,
        TypeName::Dec => ScalarType::Dec,
    };

    let result_ty = QueryType::Scalar(scalar_ty);
    let span = Span::new(type_name.span.start, name.span.end);

    Ok(TypedExpr {
        kind: TypedExprKind::TypeConstant {
            ty: type_name.clone(),
            name: name.clone(),
        },
        ty: result_ty,
        span,
    })
}

fn infer_struct_literal(
    fields: &[(Spanned<String>, Expr)],
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    let mut typed_fields = Vec::with_capacity(fields.len());
    let mut proj_fields = Vec::with_capacity(fields.len());
    let mut seen_names = HashSet::with_capacity(fields.len());

    for (name, value) in fields {
        if !seen_names.insert(name.as_str()) {
            return Err(TypeError::DuplicateStructField {
                name: name.value.clone(),
                span: name.span,
            });
        }
        let typed_value = infer(value, env)?;
        proj_fields.push(ProjectionField {
            name: name.value.clone(),
            ty: typed_value.ty.clone(),
        });
        typed_fields.push((name.clone(), typed_value));
    }

    let result_ty = QueryType::Record(RecordSource::Projection(proj_fields));
    Ok(TypedExpr {
        kind: TypedExprKind::Struct {
            fields: typed_fields,
        },
        ty: result_ty,
        span,
    })
}

fn infer_projection(
    base: &Expr,
    items: &[ProjectionItem],
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
    if items.is_empty() {
        return Err(TypeError::EmptyProjection { span });
    }

    let typed_base = infer(base, env)?;

    match &typed_base.ty {
        QueryType::Record(_) => {}
        QueryType::List(inner) if matches!(**inner, QueryType::Record(_)) => {}
        _ => {
            return Err(TypeError::ProjectionBaseNotRecord {
                got: typed_base.ty.to_string(),
                span: typed_base.span,
            });
        }
    }

    let pushed_scope = env.push_record_fields(&typed_base.ty);

    let mut typed_items = Vec::with_capacity(items.len());
    let mut seen_names: HashMap<&str, Span> = HashMap::new();

    for item in items {
        let (name, typed_value) = match (&item.alias, &item.value) {
            (Some(name), value)
            | (None, value @ Expr::Ident(name))
            | (None, value @ Expr::Field { name, .. }) => {
                let typed_value = infer(value, env)?;
                (name, typed_value)
            }
            (None, _) => {
                return Err(TypeError::UnnamedComputedField {
                    span: item.value.span(),
                });
            }
        };

        if let Some(&previous) = seen_names.get(name.as_str()) {
            return Err(TypeError::DuplicateProjectionField {
                name: name.to_string(),
                span: item.value.span(),
                previous,
            });
        }
        seen_names.insert(name.as_str(), name.span);

        typed_items.push(TypedProjectionItem {
            alias: Spanned {
                value: name.to_string(),
                span: item.value.span(),
            },
            value: typed_value,
        });
    }

    let is_list = matches!(&typed_base.ty, QueryType::List(_));
    let proj_source = RecordSource::Projection(
        typed_items
            .iter()
            .map(|t| ProjectionField {
                name: t.alias.to_string(),
                ty: t.value.ty.clone(),
            })
            .collect(),
    );
    let result_ty = if is_list {
        QueryType::List(Box::new(QueryType::Record(proj_source)))
    } else {
        QueryType::Record(proj_source)
    };

    if pushed_scope {
        env.pop_scope();
    }

    Ok(TypedExpr {
        kind: TypedExprKind::Projection {
            base: Box::new(typed_base),
            items: typed_items,
        },
        ty: result_ty,
        span,
    })
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
        (a @ QueryType::Unknown, b) => {
            *a = b.clone();
            types_compatible(a, b, schema)
        }
        (a, b @ QueryType::Unknown) => {
            *b = a.clone();
            types_compatible(a, b, schema)
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

fn record_fields_match(
    a: &mut RecordSource,
    b: &mut RecordSource,
    schema: &ValidatedSchema,
) -> bool {
    match (a, b) {
        (RecordSource::Schema(a_id), RecordSource::Schema(b_id)) => a_id == b_id,
        (RecordSource::Schema(sid), RecordSource::Projection(proj))
        | (RecordSource::Projection(proj), RecordSource::Schema(sid)) => {
            record_schema_matches_projection(*sid, proj, schema)
        }
        (RecordSource::Projection(a_proj), RecordSource::Projection(b_proj)) => {
            if a_proj.len() != b_proj.len() {
                return false;
            }
            a_proj.iter_mut().all(|a_field| {
                b_proj.iter_mut().any(|b_field| {
                    b_field.name == a_field.name
                        && types_compatible(&mut a_field.ty, &mut b_field.ty, schema)
                })
            })
        }
    }
}

fn record_schema_matches_projection(
    sid: StructId,
    proj: &mut Vec<ProjectionField>,
    schema: &ValidatedSchema,
) -> bool {
    let struct_ = &schema.structs[sid.index()];
    for proj_field in proj {
        let Some(schema_field) = struct_.fields.iter().find(|f| f.name() == proj_field.name) else {
            return false;
        };
        let mut schema_ty = field_query_type(schema_field);
        if !types_compatible(&mut schema_ty, &mut proj_field.ty, schema) {
            return false;
        }
    }
    true
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
            "filter" if matches!(inner.as_ref(), QueryType::Record(_)) => Some(
                MethodSig::scoped_fixed(QueryType::List(inner.clone()), 1, QueryType::is_bool),
            ),
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

fn check(
    expr: &mut TypedExpr,
    expected: &mut QueryType,
    env: &mut TypeEnv,
) -> Result<(), TypeError> {
    let type_mismatch = |exp: String, got: String| {
        Err(TypeError::UnexpectedType {
            expected: exp.to_string(),
            got: got.to_string(),
            span: expr.span,
        })
    };

    if !types_compatible(&mut expr.ty, expected, env.schema()) {
        return type_mismatch(expected.to_string(), expr.ty.to_string());
    }

    if expr.ty.is_unknown() {
        return Err(TypeError::AmbiguousType { span: expr.span });
    }

    match &mut expr.kind {
        TypedExprKind::Literal(_) | TypedExprKind::Ident(_) => {}
        // no propagation
        TypedExprKind::Field { base, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        // no propagation
        TypedExprKind::Method { base, args, .. } => {
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
            for arg in args {
                let mut arg_ty = arg.ty.clone();
                check(arg, &mut arg_ty, env)?;
            }
        }
        // no propagation
        TypedExprKind::Binary { lhs, rhs, .. } => {
            let mut lhs_ty = lhs.ty.clone();
            let mut rhs_ty = rhs.ty.clone();
            check(lhs, &mut lhs_ty, env)?;
            check(rhs, &mut rhs_ty, env)?;
        }
        TypedExprKind::Unary { expr: operand, .. } => {
            check(operand, &mut expr.ty, env)?;
        }
        // no propagation
        TypedExprKind::Cast { expr: operand, .. } => {
            let mut ty = operand.ty.clone();
            check(operand, &mut ty, env)?;
        }
        TypedExprKind::If { arms, default } => {
            arms.iter_mut()
                .map(|(_, b)| b)
                .chain(std::iter::once(default.as_mut()))
                .map(|b| check(b, expected, env))
                .collect::<Result<(), _>>()?;
        }
        TypedExprKind::Some { value } => {
            let QueryType::Optional(inner) = &mut expr.ty else {
                return type_mismatch("?T".into(), expr.ty.to_string());
            };
            check(value.as_mut(), inner, env)?;
        }
        TypedExprKind::Tuple { elements } => {
            let QueryType::Tuple(inners) = &mut expr.ty else {
                return type_mismatch("Tuple".into(), expr.ty.to_string());
            };
            elements
                .iter_mut()
                .zip(inners.iter_mut())
                .map(|(elem, ty)| check(elem, ty, env))
                .collect::<Result<(), _>>()?;
        }
        TypedExprKind::Array { elements } => {
            let QueryType::List(inner) = &mut expr.ty else {
                return type_mismatch("List<T>".into(), expr.ty.to_string());
            };
            elements
                .iter_mut()
                .map(|e| check(e, inner, env))
                .collect::<Result<(), _>>()?;
        }
        TypedExprKind::Struct { fields } => {
            let QueryType::Record(RecordSource::Projection(proj_fields)) = &mut expr.ty else {
                return type_mismatch("Record".into(), expr.ty.to_string());
            };
            fields
                .iter_mut()
                .map(|(name, expr)| {
                    let Some(field) = proj_fields
                        .iter_mut()
                        .find(|field| field.name == name.as_str())
                    else {
                        return type_mismatch(
                            format!("Record with field `{}`", name.as_str()),
                            expr.ty.to_string(),
                        );
                    };
                    check(expr, &mut field.ty, env)
                })
                .collect::<Result<(), _>>()?;
        }
        // no propagation
        TypedExprKind::Projection { base, items } => {
            items
                .iter_mut()
                .map(|item| {
                    let mut item_ty = item.value.ty.clone();
                    check(&mut item.value, &mut item_ty, env)
                })
                .collect::<Result<(), _>>()?;
            let mut base_ty = base.ty.clone();
            check(base, &mut base_ty, env)?;
        }
        TypedExprKind::TypeConstant { .. } => {}
    }
    Ok(())
}

fn typecheck_stmt(stmt: &Statement, env: &mut TypeEnv) -> Result<TypedStatement, TypeError> {
    match stmt {
        Statement::Expr(expr) => {
            let typed = infer(expr, env)?;
            Ok(TypedStatement::Expr(typed))
        }
        Statement::Mutation(mutation) => typecheck_mutation(mutation, env),
    }
}

fn typecheck_mutation(
    mutation: &MutationStmt,
    env: &mut TypeEnv,
) -> Result<TypedStatement, TypeError> {
    let kind = mutation.kind.value;

    if kind == MutationKind::Delete && mutation.arg.is_some() {
        return Err(TypeError::WrongArgCount {
            method: kind.to_string(),
            expected: 0,
            got: 1,
            span: mutation.kind.span,
        });
    }

    if matches!(kind, MutationKind::Insert | MutationKind::Update) && mutation.arg.is_none() {
        return Err(TypeError::WrongArgCount {
            method: kind.to_string(),
            expected: 1,
            got: 0,
            span: mutation.kind.span,
        });
    }

    match kind {
        MutationKind::Delete => {
            let typed_base = infer(&mutation.base, env)?;
            check_mutation_base(&typed_base.ty, typed_base.span)?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Delete {
                span: mutation.kind.span,
                base: typed_base,
            }))
        }
        MutationKind::Insert => {
            let (typed_base, struct_id) = check_insert_base(&mutation.base, env)?;
            let mut typed_arg = infer(mutation.arg.as_ref().expect("checked above"), env)?;
            validate_insert(struct_id, &mut typed_arg, env.schema())?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Insert {
                span: mutation.kind.span,
                base: typed_base,
                arg: typed_arg,
            }))
        }
        MutationKind::Update => {
            let typed_base = infer(&mutation.base, env)?;
            let struct_id = check_mutation_base(&typed_base.ty, typed_base.span)?;
            env.push_scope();
            env.define(
                "prev".to_string(),
                QueryType::Record(RecordSource::Schema(struct_id)),
            );
            let mut typed_arg = infer(mutation.arg.as_ref().expect("checked above"), env)?;
            env.pop_scope();
            validate_update(struct_id, &mut typed_arg, env.schema())?;
            Ok(TypedStatement::Mutation(TypedMutationStmt::Update {
                span: mutation.kind.span,
                base: typed_base,
                arg: typed_arg,
            }))
        }
    }
}

fn check_mutation_base(ty: &QueryType, span: Span) -> Result<StructId, TypeError> {
    match ty {
        QueryType::List(inner) => match inner.as_ref() {
            QueryType::Record(RecordSource::Schema(id)) => Ok(*id),
            QueryType::Record(RecordSource::Projection(_)) => {
                Err(TypeError::MutationOnProjection { span })
            }
            _ => Err(TypeError::MutationOnNonList {
                got: ty.to_string(),
                span,
            }),
        },
        _ => Err(TypeError::MutationOnNonList {
            got: ty.to_string(),
            span,
        }),
    }
}

fn check_insert_base(base: &Expr, env: &mut TypeEnv) -> Result<(TypedExpr, StructId), TypeError> {
    let base_ident = match base {
        Expr::Ident(name) => name,
        _ => {
            return Err(TypeError::InsertOnNonRoot {
                got: "expression".into(),
                span: base.span(),
            });
        }
    };
    if !env
        .schema()
        .roots
        .iter()
        .any(|r| r.name == base_ident.value)
    {
        return Err(TypeError::InsertOnNonRoot {
            got: base_ident.value.clone(),
            span: base_ident.span,
        });
    };
    let ty = env
        .resolve(&base_ident.value)
        .ok_or_else(|| TypeError::UnknownIdentifier {
            name: base_ident.value.clone(),
            span: base_ident.span,
        })?;
    let struct_id = match ty {
        QueryType::List(inner)
            if let QueryType::Record(RecordSource::Schema(id)) = inner.as_ref() =>
        {
            *id
        }
        _ => {
            return Err(TypeError::InsertOnNonRoot {
                got: base_ident.value.clone(),
                span: base_ident.span,
            });
        }
    };
    let span = base.span();
    let typed_base = TypedExpr {
        kind: TypedExprKind::Ident(base_ident.clone()),
        ty: ty.clone(),
        span,
    };
    Ok((typed_base, struct_id))
}

fn expect_constructed_record(
    ty: &mut QueryType,
    span: Span,
    mutation_kind: MutationKind,
) -> Result<&mut Vec<ProjectionField>, TypeError> {
    match ty {
        QueryType::Record(RecordSource::Projection(p)) => Ok(p),
        ty => Err(TypeError::ArgTypeMismatch {
            method: mutation_kind.to_string(),
            expected: "constructed record".into(),
            got: ty.to_string(),
            span,
        }),
    }
}

fn validate_insert(
    sid: StructId,
    typed_arg: &mut TypedExpr,
    schema: &ValidatedSchema,
) -> Result<(), TypeError> {
    let struct_ = &schema.structs[sid.index()];
    let (record_ty, is_list) = match &mut typed_arg.ty {
        QueryType::List(inner) => (inner.as_mut(), true),
        ty => (ty, false),
    };
    let proj_fields = expect_constructed_record(record_ty, typed_arg.span, MutationKind::Insert)?;

    for schema_field in &struct_.fields {
        let (expected_name, is_optional) = match schema_field {
            Field::Value {
                name,
                ty: ValueType::Optional(_),
                ..
            } => (name, true),
            Field::Value { name, .. } | Field::Array { name, .. } => (name, false),
            Field::Ref {
                name,
                owning: false,
                ..
            } => {
                if proj_fields.iter().any(|f| &f.name == name) {
                    return Err(TypeError::RefFieldInMutation {
                        field: name.clone(),
                        struct_name: struct_.name.clone(),
                        span: typed_arg.span,
                    });
                }
                continue;
            }
            Field::Ref { name, optional, .. } => (name, *optional),
        };

        if proj_fields.iter().any(|f| &f.name == expected_name) {
            continue;
        }

        if is_optional {
            let schema_ty = field_query_type(schema_field);
            let none_expr = TypedExpr {
                kind: TypedExprKind::Literal(Spanned {
                    span: typed_arg.span,
                    value: Literal::None,
                }),
                ty: schema_ty.clone(),
                span: typed_arg.span,
            };
            let mut cloned_ty = QueryType::Record(RecordSource::Projection(proj_fields.clone()));
            if is_list {
                cloned_ty = QueryType::List(Box::new(cloned_ty));
            }
            let prev_arg_kind = std::mem::replace(&mut typed_arg.kind, none_expr.kind.clone());
            let _ = std::mem::replace(
                &mut typed_arg.kind,
                TypedExprKind::Projection {
                    base: Box::new(TypedExpr {
                        kind: prev_arg_kind,
                        ty: cloned_ty,
                        span: typed_arg.span,
                    }),
                    items: vec![TypedProjectionItem {
                        alias: Spanned {
                            value: expected_name.clone(),
                            span: typed_arg.span,
                        },
                        value: none_expr,
                    }],
                },
            );
            proj_fields.push(ProjectionField {
                name: expected_name.clone(),
                ty: schema_ty,
            });
        } else {
            return Err(TypeError::InsertMissingRequiredField {
                field: expected_name.clone(),
                struct_name: struct_.name.clone(),
                span: typed_arg.span,
            });
        }
    }

    for proj_field in proj_fields {
        let schema_field = struct_.fields.iter().find(|f| match f {
            Field::Value { name, .. } | Field::Array { name, .. } | Field::Ref { name, .. } => {
                name == &proj_field.name
            }
        });
        let Some(mut schema_ty) = schema_field.map(field_query_type) else {
            return Err(TypeError::StructFieldNotInSchema {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                span: typed_arg.span,
            });
        };

        if !types_compatible(&mut schema_ty, &mut proj_field.ty, schema) {
            return Err(TypeError::StructFieldTypeMismatch {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                expected: schema_ty.to_string(),
                got: proj_field.ty.to_string(),
                span: typed_arg.span,
            });
        }
    }

    Ok(())
}

fn validate_update(
    sid: StructId,
    typed_arg: &mut TypedExpr,
    schema: &ValidatedSchema,
) -> Result<(), TypeError> {
    let struct_ = &schema.structs[sid.index()];
    let proj_fields =
        expect_constructed_record(&mut typed_arg.ty, typed_arg.span, MutationKind::Update)?;

    for proj_field in proj_fields {
        let Some(schema_field) = struct_.fields.iter().find(|f| match f {
            Field::Value { name, .. } | Field::Array { name, .. } | Field::Ref { name, .. } => {
                name == &proj_field.name
            }
        }) else {
            return Err(TypeError::StructFieldNotInSchema {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                span: typed_arg.span,
            });
        };
        if let Field::Ref { owning: false, .. } = schema_field {
            return Err(TypeError::RefFieldInMutation {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                span: typed_arg.span,
            });
        }
        let mut schema_ty = field_query_type(schema_field);

        if !types_compatible(&mut schema_ty, &mut proj_field.ty, schema) {
            return Err(TypeError::StructFieldTypeMismatch {
                field: proj_field.name.clone(),
                struct_name: struct_.name.clone(),
                expected: schema_ty.to_string(),
                got: proj_field.ty.to_string(),
                span: typed_arg.span,
            });
        }
    }
    Ok(())
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
        Ok(mut result) => {
            let mut ty = result.ty.clone();
            if let Err(err) = check(&mut result, &mut ty, &mut env) {
                diags.push(err.into());
                return (None, diags);
            }
            (Some(TypedQueryFile { statements, result }), diags)
        }
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

    #[test]
    fn if_else_basic() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if true { 1 } else { 2 }"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn if_else_bool_result() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if true { false } else { true }"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn if_else_condition_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if 42 { 1 } else { 2 }"#);
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn if_else_branch_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if true { 1 } else { "two" }"#);
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn if_else_if_chain() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"if true { "A" } else if false { "B" } else { "C" }"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn if_else_with_comparison_condition() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(
            r#"if users.first().unwrap().age > 18 { "adult" } else { "minor" }"#,
        );
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::String));
    }

    #[test]
    fn cast_int_to_float() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("42 as Float");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn cast_int_to_dec() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("42 as Dec");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn cast_float_to_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0f as Int");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn cast_float_to_dec() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0f as Dec");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn cast_dec_to_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0 as Int");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn cast_dec_to_float() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("1.0 as Float");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn cast_bool_to_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("true as Int");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn cast_disallowed_type() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#""hello" as Int"#);
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn cast_complex_expr() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users.first().unwrap().age as Float + 1.0f");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn some_basic() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"some("hello")"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::Scalar(ScalarType::String).wrap_optional()
        );
    }

    #[test]
    fn some_complex_expr() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("some(users.first().unwrap().age)");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn some_none() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("none");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Unknown.wrap_optional());
    }

    #[test]
    fn array_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("[1, 2, 3]");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int)))
        );
    }

    #[test]
    fn array_string() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"["a", "b", "c"]"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::String)))
        );
    }

    #[test]
    fn array_type_mismatch() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"[1, "two", 3]"#);
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn array_types_inferred() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("[none, some(1), none]");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
        );
        match result.kind {
            TypedExprKind::Array { elements } => {
                assert_eq!(
                    elements.into_iter().map(|e| e.ty).collect::<Vec<_>>(),
                    vec![QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::Int))); 3]
                )
            }
            _ => panic!(),
        }
    }

    #[test]
    fn array_empty() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("[]");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::List(Box::new(QueryType::Unknown)));
    }

    #[test]
    fn tuple_mixed_types() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"(1, "hello", false, @now, 1.7e-5)"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(
            result.ty,
            QueryType::Tuple(vec![
                QueryType::Scalar(ScalarType::Int),
                QueryType::Scalar(ScalarType::String),
                QueryType::Scalar(ScalarType::Bool),
                QueryType::Scalar(ScalarType::Instant),
                QueryType::Scalar(ScalarType::Dec),
            ])
        );
    }

    #[test]
    fn tuple_in_membership() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query(r#"1 in (1, 2, 3)"#);
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Bool));
    }

    #[test]
    fn projection_basic() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { name, age }");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert!(matches!(
            result.ty,
            QueryType::List(inner)
            if matches!(
                *inner,
                QueryType::Record(RecordSource::Projection(ref fields))
                if fields.len() == 2
            )
        ));
    }

    #[test]
    fn projection_single_field() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { name }");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 1);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[0].ty, QueryType::Scalar(ScalarType::String));
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_computed_field() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) =
            crate::parse_query("users { name, adult = users.first().unwrap().age > 18 }");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[1].name, "adult");
                    assert_eq!(fields[1].ty, QueryType::Scalar(ScalarType::Bool));
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_empty_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { }");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_duplicate_field_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { name, name }");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_on_non_record_error() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("42 { name }");
        let result = infer(&file.unwrap().result, &mut env);
        assert!(result.is_err());
    }

    #[test]
    fn projection_derived_alias() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users { name, profile?.username }");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        match &result.ty {
            QueryType::List(inner) => match inner.as_ref() {
                QueryType::Record(RecordSource::Projection(fields)) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].name, "name");
                    assert_eq!(fields[1].name, "username");
                    assert_eq!(
                        fields[1].ty,
                        QueryType::Optional(Box::new(QueryType::Scalar(ScalarType::String)))
                    );
                }
                other => panic!("expected Record, got {other:?}"),
            },
            other => panic!("expected List, got {other:?}"),
        }
    }

    #[test]
    fn projection_single_record_base() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("users.first().unwrap() { name, age }");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert!(matches!(
            result.ty,
            QueryType::Record(RecordSource::Projection(_))
        ));
    }

    #[test]
    fn mutation_insert() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_update() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(r#"users[age == 30].update({ age = 31 }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_update_with_prev() {
        let schema = test_schema();
        let (file, _diags) =
            crate::parse_query(r#"users[age == 30].update({ age = prev.age + 1 }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_delete() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(r#"users[!active].delete(); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_on_projection_error() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(r#"users { name }.insert({ name = "Alice" }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_on_scalar_error() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(r#"42.insert({ name = "Alice" }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_delete_arg_error() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(r#"users[!active].delete("abc"); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn type_constant_int() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("Int::MAX");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Int));
    }

    #[test]
    fn type_constant_float() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("Float::MIN");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Float));
    }

    #[test]
    fn type_constant_dec() {
        let schema = test_schema();
        let mut env = TypeEnv::new(&schema);
        let (file, _) = crate::parse_query("Dec::MAX");
        let result = infer(&file.unwrap().result, &mut env).unwrap();
        assert_eq!(result.ty, QueryType::Scalar(ScalarType::Dec));
    }

    #[test]
    fn mutation_insert_missing_required() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(r#"users.insert({ name = "Alice" }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_insert_unknown_field() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now, bogus = 1 }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_insert_wrong_type() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = 42, age = 30, score = 1.0f, balance = 0.0, active = true, created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_update_unknown_field() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(r#"users[name == "Bob"].update({ bogus = 2 }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_update_subset() {
        let schema = test_schema();
        let (file, _diags) =
            crate::parse_query(r#"users[name == "Bob"].update({ age = 30, score = 2.5f }); 0"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_duplicate_field() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, name = "Bob", created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_insert_on_non_root() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users[age == 30].insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn mutation_insert_optional_field_omitted() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn mutation_insert_with_ref_field() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now, profile = none }); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty());
    }

    #[test]
    fn file_with_mutations_result_expr() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"
            users.insert({ name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now });
            users.insert({ name = "Bob", age = 25, score = 2.0f, balance = 0.0, active = true, created = @now });
            users[!(name in ("Bob", "Alice"))].delete();
            users[name == "Bob"].update({ score = prev.score + 0.4f });
            users { name, age }
            "#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn batch_insert() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", age = 25, score = 2.0f, balance = 2.0, active = false, created = @today },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(diags.is_empty(), "expected no errors, got {diags:?}");
    }

    #[test]
    fn batch_insert_element_missing_required() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", score = 2.0f, balance = 0.0, active = true, created = @now },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn batch_insert_element_mismatched_type() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = 42, age = 25, score = 2.0f, balance = 0.0, active = true, created = @now },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn batch_insert_element_unknown_field() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            r#"users.insert([
                { name = "Alice", age = 30, score = 1.0f, balance = 0.0, active = true, created = @now },
                { name = "Bob", age = 25, score = 2.0f, balance = 0.0, active = true, created = @now, foo = 1 },
            ]); 0"#,
        );
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_if_else_optional_list() {
        let schema = test_schema();
        let (file, _diags) = crate::parse_query(
            "
            if true { none }
            else {
                if false { some([]) }
                else { some([1, 2, 3]) }
            }
            ",
        );
        let (typed, _diags) = typecheck(file.unwrap(), &schema);
        assert_eq!(
            typed.unwrap().result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int))).wrap_optional()
        );
    }

    #[test]
    fn typecheck_if_else_ambiguous() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("if true { none } else { none }");
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_array_optional() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("[none, some(3), none]");
        let (typed, _diags) = typecheck(file.unwrap(), &schema);
        assert_eq!(
            typed.unwrap().result.ty,
            QueryType::List(Box::new(QueryType::Scalar(ScalarType::Int).wrap_optional()))
        );
    }

    #[test]
    fn typecheck_array_empty() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("[]");
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_array_mismatched_types() {
        let schema = test_schema();
        let (file, _) = crate::parse_query(r#"[1, "two", 3]"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_some_none_if_else() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("if true { some(42) } else { none }");
        let (typed, _diags) = typecheck(file.unwrap(), &schema);
        assert_eq!(
            typed.unwrap().result.ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn typecheck_method_chain() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("users.first().unwrap().profile?.username");
        let (typed, _diags) = typecheck(file.unwrap(), &schema);
        assert_eq!(
            typed.unwrap().result.ty,
            QueryType::Scalar(ScalarType::String).wrap_optional()
        );
    }

    #[test]
    fn typecheck_filter_project() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("users[age > 18] { name, profile?.username }");
        let (typed, _diags) = typecheck(file.unwrap(), &schema);
        assert!(matches!(
            typed.unwrap().result.ty,
            QueryType::List(inner) if matches!(*inner, QueryType::Record(RecordSource::Projection(_)))
        ));
    }

    #[test]
    fn typecheck_optional_comparison_error() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("none == 1");
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_optional_ambiguous() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("none == none");
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }

    #[test]
    fn typecheck_filter_method_optional() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("users[age > 18].first()?.age");
        let (typed, _diags) = typecheck(file.unwrap(), &schema);
        assert_eq!(
            typed.unwrap().result.ty,
            QueryType::Scalar(ScalarType::Int).wrap_optional()
        );
    }

    #[test]
    fn typecheck_tuple() {
        let schema = test_schema();
        let (file, _) = crate::parse_query("if true { (1, 2) } else { (3, 4) }");
        let (typed, _diags) = typecheck(file.unwrap(), &schema);
        assert_eq!(
            typed.unwrap().result.ty,
            QueryType::Tuple(vec![
                QueryType::Scalar(ScalarType::Int),
                QueryType::Scalar(ScalarType::Int),
            ])
        );
    }

    #[test]
    fn typecheck_tuple_mismatch() {
        let schema = test_schema();
        let (file, _) = crate::parse_query(r#"if true { (1, 2, 4) } else { (4, "abc") }"#);
        let (_typed, diags) = typecheck(file.unwrap(), &schema);
        assert!(!diags.is_empty());
    }
}
