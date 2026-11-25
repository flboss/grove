mod access;
mod cast;
mod construct;
mod method;
mod operator;
mod row_ref;

use self::row_ref::infer_row_ref;

pub(crate) use self::row_ref::infer_mutation_arg;
use crate::ast::{Expr, Literal};
use crate::typecheck::compat::types_compatible;
use crate::typecheck::env::TypeEnv;
use crate::typecheck::error::TypeError;
use crate::typecheck::infer::access::{infer_field, infer_tuple_access};
use crate::typecheck::infer::cast::{infer_cast, infer_type_constant};
use crate::typecheck::infer::construct::{infer_projection, infer_struct_literal};
use crate::typecheck::infer::method::infer_method;
use crate::typecheck::infer::operator::{infer_binary, infer_unary};
use crate::typecheck::types::{QueryType, TypedExpr, TypedExprKind};
use grove_schema::validated::ScalarType;
use grove_types::Span;

pub(crate) fn infer_literal(literal: &Literal) -> QueryType {
    match literal {
        Literal::Int(_) => QueryType::Scalar(ScalarType::Int),
        Literal::Float(_) => QueryType::Scalar(ScalarType::Float),
        Literal::Dec(_) => QueryType::Scalar(ScalarType::Dec),
        Literal::String(_) => QueryType::Scalar(ScalarType::String),
        Literal::Bytes(_) => QueryType::Scalar(ScalarType::Bytes),
        Literal::Bool(_) => QueryType::Scalar(ScalarType::Bool),
        Literal::Instant(_) | Literal::Now | Literal::Today(_) => {
            QueryType::Scalar(ScalarType::Instant)
        }
        Literal::Duration(_) => QueryType::Scalar(ScalarType::Duration),
        Literal::None => QueryType::Unknown.wrap_optional(),
    }
}
pub(crate) fn infer(expr: &Expr, env: &mut TypeEnv) -> Result<TypedExpr, TypeError> {
    match expr {
        Expr::Literal(lit) => {
            let ty = infer_literal(lit);
            Ok(env.typed(TypedExprKind::Literal(lit.clone()), ty, lit.span))
        }
        Expr::Ident(name) => {
            let (entry, depth) = env
                .resolve(name)
                .ok_or_else(|| TypeError::UnknownIdentifier {
                    name: name.to_string(),
                    span: name.span,
                })?;
            let binding = entry.binding.clone();
            let ty = entry.ty.clone();
            let provenance = entry.provenance;
            Ok(env.typed_with_provenance(
                TypedExprKind::Ident {
                    name: name.clone(),
                    binding,
                    depth,
                },
                ty,
                provenance,
                name.span,
            ))
        }
        Expr::EnumVariant {
            enum_name,
            variant,
            span,
        } => {
            let Some((enum_id, enum_def)) = env.schema().enum_by_name(enum_name) else {
                return Err(TypeError::UnknownEnum {
                    name: enum_name.value.clone(),
                    span: enum_name.span,
                });
            };
            let Some(variant) = enum_def.variant_index(variant) else {
                return Err(TypeError::UnknownEnumVariant {
                    enum_name: enum_name.value.clone(),
                    variant: variant.value.clone(),
                    span: variant.span,
                });
            };
            Ok(env.typed(
                TypedExprKind::EnumVariant { enum_id, variant },
                QueryType::Enum(enum_id),
                *span,
            ))
        }
        Expr::Field {
            base,
            name,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_field(typed_base, name, *opt_depth, env)
        }
        Expr::TupleAccess {
            base,
            index,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_tuple_access(typed_base, index, *opt_depth, env)
        }
        Expr::Method {
            base,
            name,
            args,
            opt_depth,
            ..
        } => {
            let typed_base = infer(base, env)?;
            infer_method(typed_base, name, args, *opt_depth, env)
        }
        Expr::Binary { op, lhs, rhs, .. } => infer_binary(op, lhs, rhs, env),
        Expr::Unary { op, expr, .. } => infer_unary(op, expr, env),
        Expr::RowRef { expr, span } => infer_row_ref(expr, *span, env),
        Expr::Tuple { elements, .. } => {
            let mut typed_elems = Vec::new();
            for elem in elements {
                typed_elems.push(infer(elem, env)?);
            }
            let ty = QueryType::Tuple(typed_elems.iter().map(|e| e.ty.clone()).collect());
            Ok(env.typed(
                TypedExprKind::Tuple {
                    elements: typed_elems,
                },
                ty,
                expr.span(),
            ))
        }
        Expr::Array { elements, span } => infer_array(elements, *span, env),
        Expr::Some { value, .. } => {
            let typed_value = infer(value, env)?;
            let ty = typed_value.ty.clone().wrap_optional();
            let provenance = typed_value.provenance;
            Ok(env.typed_with_provenance(
                TypedExprKind::Some {
                    value: Box::new(typed_value),
                },
                ty,
                provenance,
                expr.span(),
            ))
        }
        Expr::If {
            arms,
            default,
            span,
        } => infer_if(arms, default, *span, env),
        Expr::Cast {
            expr, ty, fallible, ..
        } => infer_cast(expr, ty, *fallible, env),
        Expr::Projection {
            base,
            items,
            spread,
            span,
        } => infer_projection(base, items, *spread, *span, env),
        Expr::TypeConstant { ty, name } => infer_type_constant(ty, name, env),
        Expr::Struct { fields, span } => infer_struct_literal(fields, *span, env),
    }
}
pub(crate) fn infer_array(
    elements: &[Expr],
    span: Span,
    env: &mut TypeEnv,
) -> Result<TypedExpr, TypeError> {
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

    Ok(env.typed(
        TypedExprKind::Array {
            elements: typed_elems,
        },
        ty,
        span,
    ))
}
pub(crate) fn infer_if(
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
    let mut provenance = typed_default.provenance;
    for (_, body) in &typed_arms {
        provenance = provenance.merge_branches(body.provenance);
    }

    Ok(env.typed_with_provenance(
        TypedExprKind::If {
            arms: typed_arms,
            default: Box::new(typed_default),
        },
        ty,
        provenance,
        span,
    ))
}
